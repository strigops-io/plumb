-- Copyright (C) 2026 Plumb contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
-- Run ONLY after postings_demo.sql in the same disposable plumb_postings_* database.
-- Creates a NEW mutation schema, never alters/truncates the committed demo corpus.
-- COMMITS fixtures and evidence; VACUUM must run outside a transaction.
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
SET search_path=pg_catalog,public;
SET max_parallel_workers_per_gather=0;
SET jit=off;
SET work_mem='64MB';
SET synchronous_commit=on;
BEGIN;
SELECT plumb_postings_demo.check((SELECT count(*)=1 FROM plumb_postings_demo.identity
  WHERE marker='postings-demo-v1' AND database_name=current_database()
    AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())), 'mutation fixture identity');
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_namespace WHERE nspname='plumb_postings_mutations') THEN
    RAISE EXCEPTION 'Refusing: mutation schema already exists; use a new disposable database, not DROP/overwrite';
  END IF;
END $$;
CREATE SCHEMA plumb_postings_mutations;
CREATE FUNCTION plumb_postings_mutations.reject(stmt text, message_pattern text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE err text; code text;
BEGIN
  BEGIN EXECUTE stmt;
  EXCEPTION WHEN OTHERS THEN
    GET STACKED DIAGNOSTICS err=MESSAGE_TEXT, code=RETURNED_SQLSTATE;
    IF err !~* message_pattern THEN
      RAISE EXCEPTION 'Unexpected rejection [%] % for %', code,err,stmt;
    END IF;
    INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('expected rejection',
      jsonb_build_object('sql',stmt,'sqlstate',code,'message',err));
    RETURN;
  END;
  RAISE EXCEPTION 'Expected rejection but command succeeded: %',stmt;
END $$;
CREATE FUNCTION plumb_postings_mutations.validate(stage text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE q text;
BEGIN
  FOREACH q IN ARRAY ARRAY['beacon','beacon AND copper','beacon OR leftmark',
    'leftmark AND rightmark','leftmark OR rightmark','absentmarker',
    '"amber cedar"','amber AND NOT beacon','bea*'] LOOP
    PERFORM plumb_postings_demo.compare('mutation ' || stage,
      'plumb_postings_mutations.docs'::regclass,'mutation_postings',q,
      CASE WHEN q='beacon' THEN
        'SELECT id FROM plumb_postings_mutations.docs WHERE strpos(coalesce(body, ''''), ''beacon'')>0'
      ELSE NULL END);
  END LOOP;
END $$;
-- Maintenance can be implemented OR explicitly rejected by this milestone.
-- Only an explicit unsupported-operation error is accepted, never corruption/wrong results.
CREATE FUNCTION plumb_postings_mutations.maintenance(stmt text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE err text; code text;
BEGIN
  BEGIN EXECUTE stmt;
  EXCEPTION WHEN OTHERS THEN
    GET STACKED DIAGNOSTICS err=MESSAGE_TEXT,code=RETURNED_SQLSTATE;
    IF code <> '0A000' OR err !~* '(not support|unsupported|not implement)' THEN
      RAISE EXCEPTION 'Unexpected maintenance failure [%] % for %',code,err,stmt;
    END IF;
    INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('maintenance explicitly rejected',
      jsonb_build_object('sql',stmt,'sqlstate',code,'message',err));
    RETURN false;
  END;
  INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('maintenance succeeded',jsonb_build_object('sql',stmt));
  RETURN true;
END $$;
CREATE TABLE plumb_postings_mutations.docs (
  id integer PRIMARY KEY, body text, version integer NOT NULL DEFAULT 1
) WITH(fillfactor=50,autovacuum_enabled=false);
ALTER TABLE plumb_postings_mutations.docs ALTER COLUMN body SET STORAGE PLAIN;
INSERT INTO plumb_postings_mutations.docs(id,body)
  SELECT n,repeat('amber cedar maple stone ',10)
    || CASE WHEN n%2=0 THEN ' copper' ELSE ' silver' END
    || CASE WHEN n%17=0 THEN ' beacon' ELSE '' END
    || CASE n WHEN 1 THEN ' leftmark' WHEN 2 THEN ' rightmark' ELSE '' END
  FROM generate_series(1,600) n;
INSERT INTO plumb_postings_mutations.docs(id,body) VALUES(700,NULL),(701,'');
CREATE INDEX mutation_postings ON plumb_postings_mutations.docs USING plumb(body) WITH(storage='postings_v1');
ANALYZE plumb_postings_mutations.docs;
SELECT plumb_postings_mutations.validate('initial');
INSERT INTO plumb_postings_mutations.docs(id,body) VALUES(601,'amber cedar beacon copper'),(602,'amber cedar silver');
SELECT plumb_postings_mutations.validate('insert');
SAVEPOINT abandoned_insert;
INSERT INTO plumb_postings_mutations.docs(id,body) VALUES(-1,'amber cedar beacon copper'),(-2,'leftmark rightmark');
ROLLBACK TO SAVEPOINT abandoned_insert;
RELEASE SAVEPOINT abandoned_insert;
SELECT plumb_postings_demo.check(NOT EXISTS(SELECT FROM plumb_postings_mutations.docs WHERE id<0), 'aborted inserts invisible');
SELECT plumb_postings_mutations.validate('aborted insert');
-- Indexed-field changes add/remove matches and cover NULL/empty values.
UPDATE plumb_postings_mutations.docs SET body='amber cedar beacon copper' WHERE id=1;
UPDATE plumb_postings_mutations.docs SET body='amber cedar silver' WHERE id=17;
UPDATE plumb_postings_mutations.docs SET body=NULL WHERE id=34;
UPDATE plumb_postings_mutations.docs SET body='' WHERE id=51;
SELECT plumb_postings_mutations.validate('indexed updates');
-- Only version changes: eligible for HOT; reserved free space makes HOT likely, not guaranteed.
CREATE TABLE plumb_postings_mutations.hot_before AS
  SELECT id,ctid::text AS old_ctid FROM plumb_postings_mutations.docs WHERE id BETWEEN 1 AND 40;
UPDATE plumb_postings_mutations.docs SET version=version+1 WHERE id BETWEEN 1 AND 40;
SELECT plumb_postings_demo.check((SELECT count(*)=40 AND min(version)=2 AND max(version)=2
  FROM plumb_postings_mutations.docs WHERE id BETWEEN 1 AND 40), 'non-indexed version updates');
INSERT INTO plumb_postings_demo.evidence(label,detail)
 SELECT 'HOT attempt CTID movement',jsonb_build_object('updated_rows',count(*),
   'same_page_new_offset',count(*) FILTER(WHERE b.old_ctid<>d.ctid::text AND
     split_part(b.old_ctid,',',1)=split_part(d.ctid::text,',',1)),
   'note','Same-page movement is evidence of an attempt, not proof of HOT; inspect PostgreSQL stats separately')
 FROM plumb_postings_mutations.docs d JOIN plumb_postings_mutations.hot_before b USING(id);
SELECT plumb_postings_mutations.validate('non-indexed HOT attempts');
DELETE FROM plumb_postings_mutations.docs WHERE id BETWEEN 1 AND 600 AND id%17=0;
SELECT plumb_postings_mutations.validate('delete');
COMMIT;

VACUUM (ANALYZE) plumb_postings_mutations.docs;
BEGIN;
SELECT plumb_postings_mutations.validate('vacuum');
-- Reinsert stable IDs into freed space to encourage CTID reuse, retaining stale old candidates.
INSERT INTO plumb_postings_mutations.docs(id,body)
  SELECT n,'amber cedar beacon copper' FROM generate_series(17,600,17) n;
SELECT plumb_postings_mutations.validate('post-vacuum reinserts / CTID reuse attempt');
-- The persisted metapage must stay authoritative for both reads and writes.
ALTER INDEX plumb_postings_mutations.mutation_postings SET(storage='heap');
INSERT INTO plumb_postings_mutations.docs(id,body) VALUES(900,'amber cedar beacon copper');
UPDATE plumb_postings_mutations.docs SET body='amber cedar beacon silver' WHERE id=2;
DELETE FROM plumb_postings_mutations.docs WHERE id=601;
SELECT plumb_postings_mutations.validate('postings reloption changed to heap plus writes');
SELECT plumb_postings_demo.check((plumb.index_stats('plumb_postings_mutations.mutation_postings'::regclass)::jsonb
  ->>'format_version')::integer=1, 'persisted postings metadata survives storage=heap');
ALTER INDEX plumb_postings_mutations.mutation_postings SET(storage='postings_v1');
SELECT plumb_postings_mutations.maintenance('REINDEX INDEX plumb_postings_mutations.mutation_postings') AS reindex_supported;
SELECT plumb_postings_mutations.validate('reindex or explicit rejection');
SELECT plumb_postings_mutations.maintenance('TRUNCATE TABLE plumb_postings_mutations.docs') AS truncate_supported \gset
\if :truncate_supported
  SELECT plumb_postings_demo.check((SELECT count(*)=0 FROM plumb_postings_mutations.docs), 'truncate empties heap');
\endif
SELECT plumb_postings_mutations.validate('truncate or explicit rejection');
INSERT INTO plumb_postings_mutations.docs(id,body) VALUES(1001,'amber cedar beacon copper'),(1002,'amber cedar silver');
SELECT plumb_postings_mutations.validate('insert after truncate or rejection');
INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('mutation final index_stats',
  plumb.index_stats('plumb_postings_mutations.mutation_postings'::regclass)::jsonb);

-- A heap-baseline index has existing documents but no postings. A reloption flip must NOT
-- silently treat it as an empty postings index. Rejection at ALTER or read AND write is valid.
CREATE TABLE plumb_postings_mutations.baseline(id integer PRIMARY KEY,body text);
INSERT INTO plumb_postings_mutations.baseline VALUES(1,'beacon copper'),(2,'amber cedar');
CREATE INDEX baseline_heap ON plumb_postings_mutations.baseline USING plumb(body) WITH(storage='heap');
DO $$ DECLARE changed boolean := false; err text; BEGIN
  BEGIN
    ALTER INDEX plumb_postings_mutations.baseline_heap SET(storage='postings_v1');
    changed := true;
  EXCEPTION WHEN OTHERS THEN
    GET STACKED DIAGNOSTICS err=MESSAGE_TEXT;
    IF err !~* '(postings|storage|reindex|metapage|metadata)' THEN RAISE; END IF;
    INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('baseline reloption ALTER rejected',jsonb_build_object('message',err));
  END;
  IF changed THEN
    PERFORM set_config('enable_seqscan','off',true);
    PERFORM set_config('enable_bitmapscan','on',true);
    PERFORM set_config('enable_indexscan','off',true);
    PERFORM set_config('enable_indexonlyscan','off',true);
    PERFORM plumb_postings_mutations.reject(
      $q$SELECT id FROM plumb_postings_mutations.baseline WHERE body OPERATOR(pg_catalog.~~>) 'beacon'$q$,
      '(postings|storage|reindex|metapage|metadata)');
    PERFORM plumb_postings_mutations.reject(
      $q$INSERT INTO plumb_postings_mutations.baseline VALUES(3,'beacon')$q$,
      '(postings|storage|reindex|metapage|metadata)');
    ALTER INDEX plumb_postings_mutations.baseline_heap SET(storage='heap');
  END IF;
END $$;
SELECT plumb_postings_demo.compare('baseline toggle preserved','plumb_postings_mutations.baseline','baseline_heap','beacon',
  'SELECT 1 AS id');
SELECT plumb_postings_demo.check((SELECT count(*)=2 FROM plumb_postings_mutations.baseline), 'failed baseline write was atomic');

-- Whitespace is a valid nondefault tokenizer, not a made-up invalid reloption value.
SELECT plumb_postings_mutations.reject(
  $q$CREATE INDEX rejected_tokenizer ON plumb_postings_mutations.baseline USING plumb(body)
     WITH(storage='postings_v1',tokenizer='whitespace')$q$,
  '(default.*(token|analy)|(token|analy).*default|unsupported.*(token|analy)|(token|analy).*support)');
CREATE TEMP TABLE rejected_temp(body text);
SELECT plumb_postings_mutations.reject(
  'CREATE INDEX rejected_temp_idx ON rejected_temp USING plumb(body) WITH(storage=''postings_v1'')',
  '(permanent|temporary|temp table|persistence)');
CREATE UNLOGGED TABLE plumb_postings_mutations.rejected_unlogged(body text);
SELECT plumb_postings_mutations.reject(
  'CREATE INDEX rejected_unlogged_idx ON plumb_postings_mutations.rejected_unlogged USING plumb(body) WITH(storage=''postings_v1'')',
  '(permanent|unlogged|persistence)');
COMMIT;
\pset pager off
\echo 'MUTATION SUMMARY (full JSON plans retained in plumb_postings_demo.evidence):'
SELECT evidence_id,label,detail->>'seq_count' AS seq_rows,detail->>'index_count' AS index_rows,
  detail->>'oracle_count' AS oracle_rows FROM plumb_postings_demo.evidence WHERE label LIKE 'mutation %' AND detail ? 'seq_count'
  ORDER BY evidence_id;
SELECT label,jsonb_pretty(detail) FROM plumb_postings_demo.evidence
 WHERE label IN ('expected rejection','baseline reloption ALTER rejected','maintenance succeeded',
   'maintenance explicitly rejected','HOT attempt CTID movement','mutation final index_stats') ORDER BY evidence_id;
\echo 'PASS: mutation/rejection gates. HOT and CTID reuse were attempted, not claimed from timing/counters.'
\echo 'CREATE INDEX CONCURRENTLY requires the separate manual, outside-transaction check in postings-demo.md.'
