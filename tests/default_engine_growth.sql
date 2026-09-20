-- Copyright (C) 2026 Plumb contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
-- Run ONLY after postings_demo.sql in a fresh disposable checkpoint004 database.
-- psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_checkpoint004 \
--   -h /agent/workspace/default-merge-socket -p 55441 -d plumb_postings_checkpoint004 \
--   -f tests/default_engine_growth.sql
-- No existing object is dropped/replaced. Committed fixture remains for diagnosis.
\set ON_ERROR_STOP on
\timing on
\pset pager off
\if :{?disposable_db}
\else
  \echo 'Refusing: required -v disposable_db=plumb_postings_checkpoint004'
  \quit 3
\endif
SELECT current_database()=:'disposable_db'
  AND current_database()='plumb_postings_checkpoint004'
  AND inet_server_addr() IS NULL
  AND (SELECT pg_get_userbyid(datdba)=current_user FROM pg_database WHERE datname=current_database()) AS safe_database \gset
\if :safe_database
\else
  \echo 'Refusing: exact disposable database, owner and local Unix socket required'
  \quit 3
\endif
SET search_path=pg_catalog,public;
SET statement_timeout='15min';
SET lock_timeout='20s';
SET synchronous_commit=on;
SET max_parallel_workers_per_gather=0;
SET jit=off;
SET work_mem='64MB';
SELECT plumb_postings_demo.check((SELECT count(*)=1 FROM plumb_postings_demo.identity
  WHERE marker='postings-demo-v1' AND database_name=current_database()
  AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())), 'growth identity');
SELECT plumb_postings_demo.check(current_setting('fsync')='on' AND current_setting('full_page_writes')='on',
  'growth requires durable cluster');
BEGIN;
DO $$ BEGIN
  IF EXISTS(SELECT FROM pg_namespace WHERE nspname='plumb_postings_growth') THEN
    RAISE EXCEPTION 'Refusing: growth schema exists; never overwrite or drop';
  END IF;
END $$;
CREATE SCHEMA plumb_postings_growth;
CREATE TABLE plumb_postings_growth.evidence(
  evidence_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),label text NOT NULL,detail jsonb NOT NULL);
CREATE TABLE plumb_postings_growth.docs(id integer PRIMARY KEY,body text NOT NULL)
  WITH(autovacuum_enabled=false);
DO $$ DECLARE started timestamptz := clock_timestamp(); BEGIN
  INSERT INTO plumb_postings_growth.docs
    SELECT n,'amber cedar maple stone'
      || CASE WHEN n%2=0 THEN ' copper' ELSE ' silver' END
      || CASE WHEN n%997=0 THEN ' beacon' ELSE '' END
      || CASE WHEN n=1 THEN ' markerone' ELSE '' END
    FROM generate_series(1,200000) n;
  INSERT INTO plumb_postings_growth.evidence(label,detail) VALUES('load timing',
    jsonb_build_object('rows',200000,'logical_term_ctid_pairs',1000201,
      'milliseconds',extract(epoch FROM clock_timestamp()-started)*1000));
END $$;
-- The key gate: load first, then no WITH clause whatsoever on the big index.
DO $$ DECLARE started timestamptz := clock_timestamp(); BEGIN
  CREATE INDEX docs_default ON plumb_postings_growth.docs USING plumb(body);
  INSERT INTO plumb_postings_growth.evidence(label,detail) VALUES('build timing',
    jsonb_build_object('milliseconds',extract(epoch FROM clock_timestamp()-started)*1000));
END $$;
ANALYZE plumb_postings_growth.docs;
-- Unrelated reloptions must not accidentally reset the storage default to heap.
CREATE TABLE plumb_postings_growth.options_docs(id integer PRIMARY KEY,body text);
INSERT INTO plumb_postings_growth.options_docs VALUES(1,'beacon copper'),(2,'silver');
CREATE INDEX options_default ON plumb_postings_growth.options_docs USING plumb(body)
  WITH(initial_segment_count=2,k1=1.3,b=0.7);
CREATE TABLE plumb_postings_growth.heap_docs(id integer PRIMARY KEY,body text);
INSERT INTO plumb_postings_growth.heap_docs VALUES(1,'beacon copper'),(2,'silver');
CREATE INDEX explicit_heap ON plumb_postings_growth.heap_docs USING plumb(body) WITH(storage='heap');
COMMIT;

BEGIN;
SELECT plumb_postings_demo.check((SELECT count(*)=200000 FROM plumb_postings_growth.docs),'200000 preloaded rows');
SELECT plumb_postings_demo.check(200000::bigint*5+200+1>262144,'corpus exceeds former cumulative build pair limit');
SELECT plumb_postings_demo.check((SELECT reloptions IS NULL FROM pg_class
  WHERE oid='plumb_postings_growth.docs_default'::regclass),'big index has no reloptions');
DO $$ DECLARE s jsonb; options_s jsonb; heap_s jsonb; BEGIN
  s := plumb.index_stats('plumb_postings_growth.docs_default');
  options_s := plumb.index_stats('plumb_postings_growth.options_default');
  heap_s := plumb.index_stats('plumb_postings_growth.explicit_heap');
  PERFORM plumb_postings_demo.check(s->>'storage'='postings_v1' AND (s->>'format_version')::int=1,
    'no-WITH default persisted v1');
  PERFORM plumb_postings_demo.check((s->>'segments')::bigint>1,'bounded build produced multiple segments');
  PERFORM plumb_postings_demo.check((s->>'payload_bytes')::bigint>0 AND (s->>'relation_blocks')::bigint>1,
    'default persisted nonzero bytes');
  PERFORM plumb_postings_demo.check((s->>'relation_blocks')::bigint=
    pg_relation_size('plumb_postings_growth.docs_default')/current_setting('block_size')::bigint,
    'relation_blocks matches physical size');
  PERFORM plumb_postings_demo.check(options_s->>'storage'='postings_v1'
    AND (options_s->>'format_version')::int=1 AND (options_s->>'payload_bytes')::bigint>0,
    'unrelated options preserve v1 default');
  PERFORM plumb_postings_demo.check(heap_s->>'storage'='heap' AND (heap_s->>'format_version')::int=0,
    'explicit heap compatibility');
  INSERT INTO plumb_postings_growth.evidence(label,detail) VALUES
    ('before merge stats',s),('unrelated options stats',options_s),('explicit heap stats',heap_s);
END $$;
SELECT plumb_postings_demo.compare('growth rare','plumb_postings_growth.docs','docs_default','beacon',
  'SELECT n AS id FROM generate_series(997,200000,997) n');
-- Bounded query materialization: beacon=200, copper=100000, not two common 200000-hit terms.
SELECT plumb_postings_demo.compare('growth AND','plumb_postings_growth.docs','docs_default','beacon AND copper',
  'SELECT n AS id FROM generate_series(997,200000,997) n WHERE n%2=0');
SELECT plumb_postings_demo.compare('growth OR','plumb_postings_growth.docs','docs_default','beacon OR markerone',
  'SELECT n AS id FROM generate_series(997,200000,997) n UNION ALL SELECT 1');
SELECT plumb_postings_demo.compare('growth options','plumb_postings_growth.options_docs','options_default','beacon',
  'SELECT 1 AS id');
SELECT plumb_postings_demo.compare('growth heap','plumb_postings_growth.heap_docs','explicit_heap','beacon',
  'SELECT 1 AS id');
SELECT plumb_postings_demo.check((SELECT (detail->>'oracle_count')::int=200
  AND (detail->>'index_count')::int=200 FROM plumb_postings_demo.evidence
  WHERE label='growth rare: beacon' ORDER BY evidence_id DESC LIMIT 1),'exact rare count 200');
DO $$ DECLARE p jsonb; BEGIN
  EXECUTE $q$EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) SELECT id FROM plumb_postings_growth.docs
    WHERE body OPERATOR(pg_catalog.~~>) 'beacon'$q$ INTO p;
  INSERT INTO plumb_postings_growth.evidence(label,detail) VALUES('natural rare plan',p);
END $$;
-- Capacity refusal is part of this checkpoint's contract, not an optional result.
-- merge_payloads passes remaining cumulative pairs to codec group/page/posting limits.
-- Which exact codec count rejects first depends on the bounded build boundary.
DO $$ DECLARE before_s jsonb; after_s jsonb; failed boolean := false;
  message text; state text; started timestamptz := clock_timestamp(); BEGIN
  before_s := plumb.index_stats('plumb_postings_growth.docs_default');
  PERFORM plumb_postings_demo.check((before_s->>'payload_bytes')::bigint<16777216,
    'pair-cap fixture stays below aggregate encoded-byte cap');
  BEGIN
    PERFORM plumb.merge_index('plumb_postings_growth.docs_default');
  EXCEPTION WHEN OTHERS THEN
    GET STACKED DIAGNOSTICS message=MESSAGE_TEXT,state=RETURNED_SQLSTATE;
    IF message NOT IN (
      'plumb postings merge transform failed: postings_v1 frame: posting count exceeds the limit',
      'plumb postings merge transform failed: postings_v1 frame: group count exceeds the limit',
      'plumb postings merge transform failed: postings_v1 frame: page count exceeds the limit') THEN
      RAISE;
    END IF;
    failed := true;
  END;
  PERFORM plumb_postings_demo.check(failed,'large merge MUST refuse cumulative decoded-pair cap; success is failure');
  after_s := plumb.index_stats('plumb_postings_growth.docs_default');
  PERFORM plumb_postings_demo.check(before_s=after_s,'failed merge leaves all exported metadata and physical size unchanged');
  INSERT INTO plumb_postings_growth.evidence(label,detail) VALUES('merge pair-cap refusal',
    jsonb_build_object('message',message,'sqlstate',state,'before',before_s,'after',after_s,
      'milliseconds',extract(epoch FROM clock_timestamp()-started)*1000));
END $$;
SELECT plumb_postings_demo.compare('growth after refusal rare','plumb_postings_growth.docs','docs_default','beacon',
  'SELECT n AS id FROM generate_series(997,200000,997) n');
SELECT plumb_postings_demo.compare('growth after refusal AND','plumb_postings_growth.docs','docs_default','beacon AND copper',
  'SELECT n AS id FROM generate_series(997,200000,997) n WHERE n%2=0');
SELECT plumb_postings_demo.compare('growth after refusal OR','plumb_postings_growth.docs','docs_default','beacon OR markerone',
  'SELECT n AS id FROM generate_series(997,200000,997) n UNION ALL SELECT 1');
INSERT INTO plumb_postings_growth.evidence(label,detail)
  SELECT label,detail FROM plumb_postings_demo.evidence WHERE label LIKE 'growth %' ORDER BY evidence_id;
COMMIT;
SELECT evidence_id,label,detail FROM plumb_postings_growth.evidence ORDER BY evidence_id;
\echo 'PASS growth: default v1, bounded multi-segment build, rare=200, AND=100, OR=201; merge cap refuses without mutation.'
