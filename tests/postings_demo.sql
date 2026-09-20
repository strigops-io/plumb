-- Copyright (C) 2026 Plumb contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
-- DISPOSABLE DATABASE ONLY. This script COMMITS the corpus and index for recovery tests.
-- psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_NAME \
--   -d plumb_postings_NAME -f tests/postings_demo.sql
-- After reconnect/recovery: add -v verify_only=on (does not rebuild the corpus/index).
\set ON_ERROR_STOP on
\if :{?disposable_db}
\else
  \echo 'Refusing: supply -v disposable_db=plumb_postings_NAME.'
  \quit 3
\endif
SELECT current_database() = :'disposable_db'
   AND current_database() LIKE 'plumb\_postings\_%' ESCAPE '\' AS safe_database \gset
\if :safe_database
\else
  \echo 'Refusing: connection must match disposable_db and start with plumb_postings_.'
  \quit 3
\endif
\if :{?verify_only}
\else
  \set verify_only off
\endif
SET search_path = pg_catalog, public;
SET max_parallel_workers_per_gather = 0;
SET jit = off;
SET work_mem = '64MB';
SET synchronous_commit = on;
\if :verify_only
  \echo 'VERIFY ONLY: reusing committed corpus/index, never rebuilding them.'
  DO $$ BEGIN
    IF NOT EXISTS (SELECT FROM pg_namespace WHERE nspname='plumb_postings_demo') THEN
      RAISE EXCEPTION 'Run the fresh-database demo first';
    END IF;
  END $$;
\else
  \echo 'WARNING: this run COMMITS permanent test data/index; use a fresh disposable cluster/database.'
  BEGIN;
  DO $$ BEGIN
    IF EXISTS (SELECT FROM pg_extension WHERE extname <> 'plpgsql')
       OR EXISTS (SELECT FROM pg_namespace WHERE nspname NOT IN ('public','pg_catalog','information_schema')
                  AND nspname NOT LIKE 'pg_toast%' AND nspname NOT LIKE 'pg_temp%')
       OR EXISTS (SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                  WHERE n.nspname NOT IN ('pg_catalog','information_schema')
                    AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%')
       OR EXISTS (SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public')
    THEN RAISE EXCEPTION 'Refusing: NEW empty database required (only plpgsql allowed)'; END IF;
  END $$;
  CREATE EXTENSION plumb;
  CREATE SCHEMA plumb_postings_demo;
  CREATE TABLE plumb_postings_demo.identity (
    marker text PRIMARY KEY, database_name name NOT NULL, database_oid oid NOT NULL
  );
  INSERT INTO plumb_postings_demo.identity
    SELECT 'postings-demo-v1', current_database(), oid FROM pg_database WHERE datname=current_database();
  CREATE TABLE plumb_postings_demo.evidence (
    evidence_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    label text NOT NULL, detail jsonb NOT NULL
  );
  CREATE FUNCTION plumb_postings_demo.check(ok boolean, label text) RETURNS void LANGUAGE plpgsql AS $$
  BEGIN
    IF ok IS DISTINCT FROM true THEN RAISE EXCEPTION 'check failed: %', label; END IF;
    RAISE NOTICE 'PASS: %', label;
  END $$;
  CREATE FUNCTION plumb_postings_demo.plan_nodes(plan jsonb) RETURNS SETOF jsonb
    LANGUAGE sql IMMUTABLE AS $$
    WITH RECURSIVE nodes(node) AS (
      SELECT plan->0->'Plan'
      UNION ALL
      SELECT child FROM nodes CROSS JOIN LATERAL jsonb_array_elements(node->'Plans') child
    ) SELECT node FROM nodes
  $$;
  -- Dynamic EXECUTE replans after each planner GUC change (no cached-plan ambiguity).
  -- Oracle SQL is controlled by this test, not external/user-provided input.
  CREATE FUNCTION plumb_postings_demo.compare(
    stage text, rel regclass, expected_index text, query text, oracle_sql text DEFAULT NULL
  ) RETURNS void LANGUAGE plpgsql AS $$
  DECLARE stmt text; seq_ids bigint[]; idx_ids bigint[]; oracle_ids bigint[];
          seq_plan jsonb; idx_plan jsonb; different boolean;
  BEGIN
    stmt := format('SELECT id::bigint FROM %s WHERE body OPERATOR(pg_catalog.~~>) %L', rel, query);
    PERFORM set_config('enable_seqscan','on',true);
    PERFORM set_config('enable_bitmapscan','off',true);
    PERFORM set_config('enable_indexscan','off',true);
    PERFORM set_config('enable_indexonlyscan','off',true);
    EXECUTE 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' || stmt INTO seq_plan;
    PERFORM plumb_postings_demo.check(EXISTS (
      SELECT FROM plumb_postings_demo.plan_nodes(seq_plan) n
      WHERE n->>'Node Type'='Seq Scan' AND n->>'Relation Name'=(SELECT relname FROM pg_class WHERE oid=rel)
    ), stage || ': reference uses Seq Scan: ' || query);
    EXECUTE 'SELECT coalesce(array_agg(id), ARRAY[]::bigint[]) FROM (' || stmt || ') s' INTO seq_ids;
    PERFORM set_config('enable_seqscan','off',true);
    PERFORM set_config('enable_bitmapscan','on',true);
    -- Leave ordinary index scans disabled: this gate specifically tests bitmap paths.
    EXECUTE 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' || stmt INTO idx_plan;
    PERFORM plumb_postings_demo.check(EXISTS (
      SELECT FROM plumb_postings_demo.plan_nodes(idx_plan) n
      WHERE n->>'Node Type'='Bitmap Index Scan' AND n->>'Index Name'=expected_index
    ), stage || ': forced Bitmap Index Scan uses ' || expected_index || ': ' || query);
    EXECUTE 'SELECT coalesce(array_agg(id), ARRAY[]::bigint[]) FROM (' || stmt || ') s' INTO idx_ids;
    SELECT EXISTS (
      (SELECT unnest(seq_ids) EXCEPT ALL SELECT unnest(idx_ids))
      UNION ALL (SELECT unnest(idx_ids) EXCEPT ALL SELECT unnest(seq_ids))
    ) INTO different;
    PERFORM plumb_postings_demo.check(NOT different, stage || ': full IDs match both ways: ' || query);
    IF oracle_sql IS NOT NULL THEN
      EXECUTE 'SELECT coalesce(array_agg(id::bigint), ARRAY[]::bigint[]) FROM (' || oracle_sql || ') o' INTO oracle_ids;
      SELECT EXISTS (
        (SELECT unnest(seq_ids) EXCEPT ALL SELECT unnest(oracle_ids))
        UNION ALL (SELECT unnest(oracle_ids) EXCEPT ALL SELECT unnest(seq_ids))
      ) INTO different;
      PERFORM plumb_postings_demo.check(NOT different, stage || ': independent oracle: ' || query);
    END IF;
    INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES(stage || ': ' || query,
      jsonb_build_object('relation',rel::text,'query',query,'seq_count',cardinality(seq_ids),
        'index_count',cardinality(idx_ids),'oracle_count',cardinality(oracle_ids),
        'seq_plan',seq_plan,'index_plan',idx_plan));
    PERFORM set_config('enable_seqscan','on',true);
    PERFORM set_config('enable_bitmapscan','on',true);
    PERFORM set_config('enable_indexscan','on',true);
    PERFORM set_config('enable_indexonlyscan','on',true);
  END $$;

  CREATE TABLE plumb_postings_demo.docs (
    id integer PRIMARY KEY, body text, version integer NOT NULL DEFAULT 1
  );
  -- PLAIN prevents future compression changes from shrinking this corpus below the group boundary.
  ALTER TABLE plumb_postings_demo.docs ALTER COLUMN body SET STORAGE PLAIN;
  INSERT INTO plumb_postings_demo.docs(id,body)
    SELECT n, repeat('amber cedar maple stone ',44)
       || CASE WHEN n%2=0 THEN ' copper' ELSE ' silver' END
       || CASE WHEN n%997=0 THEN ' beacon' ELSE '' END
       || CASE n WHEN 1 THEN ' leftmark' WHEN 2 THEN ' rightmark' ELSE '' END
    FROM generate_series(1,30000) n;
  INSERT INTO plumb_postings_demo.docs(id,body) VALUES (30001,NULL),(30002,'');
  CREATE INDEX docs_postings ON plumb_postings_demo.docs USING plumb(body) WITH(storage='postings_v1');
  ANALYZE plumb_postings_demo.docs;
  COMMIT;
\endif

-- Validation is separately committed. If it fails the already committed corpus remains for diagnosis.
BEGIN;
SELECT plumb_postings_demo.check((SELECT count(*)=1 FROM plumb_postings_demo.identity
  WHERE marker='postings-demo-v1' AND database_name=current_database()
    AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())), 'fixture identity');
SELECT plumb_postings_demo.check(current_setting('fsync')='on' AND current_setting('full_page_writes')='on',
  'recovery test requires fsync and full_page_writes enabled');
SELECT plumb_postings_demo.check((SELECT a.amname='plumb' AND c.relpersistence='p'
  FROM pg_class c JOIN pg_am a ON a.oid=c.relam WHERE c.oid='plumb_postings_demo.docs_postings'::regclass),
  'permanent plumb index');
SELECT plumb_postings_demo.check((SELECT relpersistence='p' FROM pg_class
  WHERE oid='plumb_postings_demo.docs'::regclass), 'permanent heap');
SELECT plumb_postings_demo.check((SELECT count(*)=30002 AND min(id)=1 AND max(id)=30002
  AND min(version)=1 AND max(version)=1 FROM plumb_postings_demo.docs), 'committed corpus cardinality/version');
SELECT plumb_postings_demo.check(pg_relation_size('plumb_postings_demo.docs') /
  current_setting('block_size')::bigint > 256, 'heap spans multiple 256-block groups');
SELECT plumb_postings_demo.check((SELECT count(DISTINCT split_part(trim(both '()' FROM ctid::text),',',1)::bigint/256)>1
  FROM plumb_postings_demo.docs WHERE id BETWEEN 1 AND 30000 AND id%997=0), 'rare hits span CTID groups');
SELECT plumb_postings_demo.check((SELECT count(DISTINCT split_part(trim(both '()' FROM ctid::text),',',1))=1
  AND count(DISTINCT split_part(trim(both '()' FROM ctid::text),',',2))=2
  FROM plumb_postings_demo.docs WHERE id IN (1,2)), 'disjoint terms on same heap page, distinct tuple offsets');
DO $$ DECLARE s jsonb; BEGIN
  s := plumb.index_stats('plumb_postings_demo.docs_postings'::regclass)::jsonb;
  PERFORM plumb_postings_demo.check((s->>'format_version')::integer=1, 'index format_version = 1');
  PERFORM plumb_postings_demo.check((s->>'segments')::bigint=1, 'one initial bulk segment');
  PERFORM plumb_postings_demo.check((s->>'payload_bytes')::bigint>0 AND (s->>'relation_blocks')::bigint>1,
    'nonzero persistent posting bytes/blocks');
  PERFORM plumb_postings_demo.check((s->>'relation_blocks')::bigint =
    pg_relation_size('plumb_postings_demo.docs_postings')/current_setting('block_size')::bigint,
    'stats relation_blocks matches MAIN fork size');
  INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('index_stats',s);
END $$;
-- Natural plan is observational, not forced or required to choose the index.
SET LOCAL enable_seqscan=on;
SET LOCAL enable_bitmapscan=on;
SET LOCAL enable_indexscan=on;
SET LOCAL enable_indexonlyscan=on;
DO $$ DECLARE p jsonb; BEGIN
  EXECUTE $q$EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
    SELECT id FROM plumb_postings_demo.docs WHERE body OPERATOR(pg_catalog.~~>) 'beacon'$q$ INTO p;
  INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('rare natural plan',p);
END $$;
SELECT plumb_postings_demo.compare('demo rare','plumb_postings_demo.docs','docs_postings','beacon',
  'SELECT n AS id FROM generate_series(997,30000,997) n');
SELECT plumb_postings_demo.compare('demo AND','plumb_postings_demo.docs','docs_postings','beacon AND copper',
  'SELECT n AS id FROM generate_series(997,30000,997) n WHERE n%2=0');
SELECT plumb_postings_demo.compare('demo OR','plumb_postings_demo.docs','docs_postings','beacon OR leftmark',
  'SELECT n AS id FROM generate_series(997,30000,997) n UNION ALL SELECT 1');
SELECT plumb_postings_demo.compare('same page AND','plumb_postings_demo.docs','docs_postings','leftmark AND rightmark',
  'SELECT 1 AS id WHERE false');
SELECT plumb_postings_demo.compare('same page OR','plumb_postings_demo.docs','docs_postings','leftmark OR rightmark',
  'SELECT n AS id FROM generate_series(1,2) n');
SELECT plumb_postings_demo.compare('unknown','plumb_postings_demo.docs','docs_postings','absentmarker',
  'SELECT 1 AS id WHERE false');
-- These intentionally test inherited fallback semantics, not exact bitmap pruning.
SELECT plumb_postings_demo.compare('fallback phrase','plumb_postings_demo.docs','docs_postings','"amber cedar"');
SELECT plumb_postings_demo.compare('fallback NOT','plumb_postings_demo.docs','docs_postings','amber AND NOT beacon');
SELECT plumb_postings_demo.compare('fallback wildcard','plumb_postings_demo.docs','docs_postings','bea*');
DO $$ DECLARE p jsonb; heap_node jsonb; heap_blocks bigint; BEGIN
  SELECT detail->'index_plan' INTO STRICT p FROM plumb_postings_demo.evidence
    WHERE label='demo rare: beacon' ORDER BY evidence_id DESC LIMIT 1;
  SELECT n INTO STRICT heap_node FROM plumb_postings_demo.plan_nodes(p) n
    WHERE n->>'Node Type'='Bitmap Heap Scan' AND n->>'Relation Name'='docs';
  heap_blocks := pg_relation_size('plumb_postings_demo.docs')/current_setting('block_size')::bigint;
  PERFORM plumb_postings_demo.check((heap_node->>'Exact Heap Blocks')::bigint>0
    AND (heap_node->>'Exact Heap Blocks')::bigint < heap_blocks/10.0, 'rare exact heap blocks < total heap / 10');
  PERFORM plumb_postings_demo.check((heap_node->>'Lossy Heap Blocks')::bigint=0, 'rare Lossy Heap Blocks = 0');
  PERFORM plumb_postings_demo.check((heap_node->>'Actual Rows')::bigint=30, 'rare arithmetic oracle = 30 rows');
  INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('rare summary',
    jsonb_build_object('expected_rows',30,'heap_blocks',heap_blocks,
      'exact_heap_blocks',heap_node->'Exact Heap Blocks','lossy_heap_blocks',heap_node->'Lossy Heap Blocks'));
END $$;
COMMIT;
\pset pager off
\echo 'KEY JSON (also retained in plumb_postings_demo.evidence):'
SELECT label,jsonb_pretty(detail) FROM plumb_postings_demo.evidence
 WHERE label IN ('index_stats','rare natural plan','demo rare: beacon','rare summary') ORDER BY evidence_id;
\echo 'READABLE COMPARISON SUMMARY:'
SELECT evidence_id,label,detail->>'seq_count' AS sequential_rows,detail->>'index_count' AS index_rows,
  detail->>'oracle_count' AS oracle_rows FROM plumb_postings_demo.evidence WHERE detail ? 'seq_count' ORDER BY evidence_id;
\echo 'PASS: committed postings demo. Reconnect with verify_only=on; do not rebuild before recovery verification.'
