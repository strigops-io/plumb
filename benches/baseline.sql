-- SPDX-License-Identifier: AGPL-3.0-or-later
-- Run in a fresh psql session against a disposable development database with
-- Plumb's extension already installed. No extension/schema is created here.
\set ON_ERROR_STOP on
\pset pager off
\pset format unaligned
\pset tuples_only on
\timing on
\if :{?rows}
\else
  \set rows 100000
\endif
\if :{?iterations}
\else
  \set iterations 1
\endif
\if :{?build_label}
\else
  \set build_label 'unrecorded (supply -v build_label=...)'
\endif

BEGIN;
SET LOCAL search_path = pg_catalog, pg_temp;
SET LOCAL work_mem = '64MB';
SET LOCAL jit = off;
SET LOCAL max_parallel_workers_per_gather = 0;

CREATE TEMP TABLE plumb_bench_config (
    rows integer NOT NULL CHECK (rows > 0),
    iterations integer NOT NULL CHECK (iterations BETWEEN 1 AND 100),
    build_label text NOT NULL
) ON COMMIT DROP;
INSERT INTO pg_temp.plumb_bench_config
VALUES (:'rows'::integer, :'iterations'::integer, :'build_label');

DO $check$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_extension WHERE extname = 'plumb') THEN
        RAISE EXCEPTION 'Install Plumb in a disposable development database before running this script';
    END IF;
END;
$check$;

\echo environment_json
SELECT jsonb_build_object(
    'record', 'environment',
    'started_at', clock_timestamp(),
    'postgres_version', version(),
    'server_version_num', current_setting('server_version_num'),
    'database', current_database(),
    'extension', (SELECT jsonb_build_object('name', e.extname,
        'version', e.extversion, 'schema', n.nspname)
        FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
        WHERE e.extname = 'plumb'),
    'parameters', (SELECT to_jsonb(c) FROM pg_temp.plumb_bench_config c),
    'settings', (SELECT jsonb_object_agg(name, jsonb_build_object(
        'setting', setting, 'unit', unit, 'source', source))
        FROM pg_settings WHERE name IN (
            'server_encoding', 'lc_collate', 'lc_ctype', 'block_size',
            'shared_buffers', 'temp_buffers', 'work_mem', 'maintenance_work_mem',
            'effective_cache_size', 'random_page_cost', 'seq_page_cost',
            'cpu_tuple_cost', 'cpu_operator_cost', 'default_statistics_target',
            'jit', 'max_parallel_workers_per_gather', 'track_io_timing',
            'statement_timeout', 'temp_file_limit', 'synchronous_commit'))
);

\echo setup_generate_corpus
CREATE TEMP TABLE plumb_bench_docs (
    id bigint NOT NULL,
    body text NOT NULL
) ON COMMIT DROP;
INSERT INTO pg_temp.plumb_bench_docs
SELECT n,
       'document development search baseline'
       || CASE WHEN n % 2 = 0 THEN ' common' ELSE '' END
       || CASE WHEN n % 997 = 0 THEN ' rare' ELSE '' END
       || CASE WHEN n % 101 = 0 THEN ' medium' ELSE '' END
       || repeat(' stable filler text', 8)
FROM pg_temp.plumb_bench_config c
CROSS JOIN LATERAL generate_series(1::bigint, c.rows::bigint) AS g(n);

\echo setup_build_plumb_index
CREATE INDEX plumb_bench_docs_search ON pg_temp.plumb_bench_docs USING plumb (body);
\echo setup_analyze
ANALYZE pg_temp.plumb_bench_docs;

\echo relation_sizes_json
SELECT jsonb_build_object(
    'record', 'relation_sizes',
    'heap_main_bytes', pg_relation_size('pg_temp.plumb_bench_docs'),
    'table_including_toast_bytes', pg_table_size('pg_temp.plumb_bench_docs'),
    'plumb_index_bytes', pg_relation_size('pg_temp.plumb_bench_docs_search'),
    'total_relation_bytes', pg_total_relation_size('pg_temp.plumb_bench_docs'),
    'estimated_rows', c.reltuples,
    'heap_pages', c.relpages,
    'index_definition', pg_get_indexdef('pg_temp.plumb_bench_docs_search'::regclass)
) FROM pg_class c WHERE c.oid = 'pg_temp.plumb_bench_docs'::regclass;

CREATE TEMP TABLE plumb_bench_cases (
    ordinal integer, name text, query text, expected_predicate text
) ON COMMIT DROP;
INSERT INTO pg_temp.plumb_bench_cases VALUES
    (1, 'rare_term', 'rare', 'id % 997 = 0'),
    (2, 'common_term', 'common', 'id % 2 = 0'),
    (3, 'and_terms', 'rare AND common', 'id % 997 = 0 AND id % 2 = 0'),
    (4, 'or_terms', 'rare OR medium', 'id % 997 = 0 OR id % 101 = 0');
CREATE TEMP TABLE plumb_bench_plans (
    iteration integer, case_name text, requested_path text, plan jsonb
) ON COMMIT DROP;

-- EXPLAIN executes SELECT id without returning millions of IDs to the client.
-- Alternate path order between iterations to expose (not eliminate) order bias.
-- Planner enable flags discourage paths; the JSON plan is the authority.
\echo measure_explain_analyze
DO $measure$
DECLARE
    i integer;
    q record;
    path text;
    captured jsonb;
BEGIN
    FOR i IN 1..(SELECT iterations FROM pg_temp.plumb_bench_config) LOOP
        FOR q IN SELECT * FROM pg_temp.plumb_bench_cases ORDER BY ordinal LOOP
            FOREACH path IN ARRAY CASE WHEN i % 2 = 1
                THEN ARRAY['seq', 'bitmap'] ELSE ARRAY['bitmap', 'seq'] END LOOP
                PERFORM set_config('enable_seqscan', (path = 'seq')::text, true);
                PERFORM set_config('enable_bitmapscan', (path = 'bitmap')::text, true);
                PERFORM set_config('enable_indexscan', 'off', true);
                PERFORM set_config('enable_indexonlyscan', 'off', true);
                EXECUTE format(
                    'EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON) '
                    'SELECT id FROM pg_temp.plumb_bench_docs WHERE body ~~> %L',
                    q.query) INTO captured;
                INSERT INTO pg_temp.plumb_bench_plans VALUES (i, q.name, path, captured);
            END LOOP;
        END LOOP;
    END LOOP;
END;
$measure$;

\echo plans_json_one_record_per_sample
SELECT jsonb_build_object('record', 'plan', 'iteration', iteration,
    'case', case_name, 'requested_path', requested_path,
    'observed_root_node', plan->0->'Plan'->>'Node Type', 'explain', plan)
FROM pg_temp.plumb_bench_plans ORDER BY iteration, case_name, requested_path;

-- Keep only one case's results at a time. EXCEPT ALL checks complete ID
-- multisets in both directions, not counts/hashes; it can spill to disk.
CREATE TEMP TABLE plumb_bench_seq (id bigint) ON COMMIT DROP;
CREATE TEMP TABLE plumb_bench_bitmap (id bigint) ON COMMIT DROP;
CREATE TEMP TABLE plumb_bench_checks (case_name text, matched_rows bigint)
    ON COMMIT DROP;
\echo correctness_check_full_id_multisets
DO $correctness$
DECLARE
    q record;
    mismatch boolean;
BEGIN
    FOR q IN SELECT * FROM pg_temp.plumb_bench_cases ORDER BY ordinal LOOP
        TRUNCATE pg_temp.plumb_bench_seq, pg_temp.plumb_bench_bitmap;
        PERFORM set_config('enable_seqscan', 'on', true);
        PERFORM set_config('enable_bitmapscan', 'off', true);
        PERFORM set_config('enable_indexscan', 'off', true);
        PERFORM set_config('enable_indexonlyscan', 'off', true);
        EXECUTE format('INSERT INTO pg_temp.plumb_bench_seq '
            'SELECT id FROM pg_temp.plumb_bench_docs WHERE body ~~> %L', q.query);
        PERFORM set_config('enable_seqscan', 'off', true);
        PERFORM set_config('enable_bitmapscan', 'on', true);
        EXECUTE format('INSERT INTO pg_temp.plumb_bench_bitmap '
            'SELECT id FROM pg_temp.plumb_bench_docs WHERE body ~~> %L', q.query);

        -- Restore normal scan choices for the comparison work itself.
        PERFORM set_config('enable_seqscan', 'on', true);
        PERFORM set_config('enable_bitmapscan', 'on', true);
        SELECT EXISTS (
            (SELECT id FROM pg_temp.plumb_bench_seq
             EXCEPT ALL SELECT id FROM pg_temp.plumb_bench_bitmap)
            UNION ALL
            (SELECT id FROM pg_temp.plumb_bench_bitmap
             EXCEPT ALL SELECT id FROM pg_temp.plumb_bench_seq)
        ) INTO mismatch;
        IF mismatch THEN
            RAISE EXCEPTION 'ID multiset mismatch between seq/bitmap requests: %', q.name;
        END IF;

        -- Independent arithmetic oracle also catches two equally wrong paths.
        EXECUTE format('SELECT EXISTS ('
            '(SELECT id FROM pg_temp.plumb_bench_seq EXCEPT ALL '
            ' SELECT id FROM pg_temp.plumb_bench_docs WHERE %s) UNION ALL '
            '(SELECT id FROM pg_temp.plumb_bench_docs WHERE %s EXCEPT ALL '
            ' SELECT id FROM pg_temp.plumb_bench_seq))',
            q.expected_predicate, q.expected_predicate) INTO mismatch;
        IF mismatch THEN
            RAISE EXCEPTION 'ID multiset mismatch against generated-data oracle: %', q.name;
        END IF;
        INSERT INTO pg_temp.plumb_bench_checks
        SELECT q.name, count(*) FROM pg_temp.plumb_bench_seq;
    END LOOP;
END;
$correctness$;

\echo correctness_json
SELECT jsonb_build_object('record', 'correctness', 'case', case_name,
    'matched_rows', matched_rows, 'id_multisets_equal', true,
    'arithmetic_oracle_equal', true)
FROM pg_temp.plumb_bench_checks ORDER BY case_name;

-- All created relations are session-local temporary objects. SET LOCAL and all
-- objects above are rolled back; no existing user relation is altered/dropped.
ROLLBACK;
\echo baseline_complete_rolled_back
