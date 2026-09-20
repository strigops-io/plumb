-- Copyright (C) 2026 Plumb contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
-- Run AFTER a deliberately authorized immediate-stop/restart of the disposable
-- cluster, and after postings_demo.sql verify_only plus mutation verification.
-- This script does not manage/restart PostgreSQL and never rebuilds an index.
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
  \echo 'Refusing: database name/connection mismatch.'
  \quit 3
\endif
BEGIN;
SET LOCAL search_path=pg_catalog,public;
SET LOCAL jit=off;
SET LOCAL max_parallel_workers_per_gather=0;
SET LOCAL work_mem='64MB';
SELECT plumb_postings_demo.check((SELECT count(*)=1 FROM plumb_postings_demo.identity
 WHERE marker='postings-demo-v1' AND database_name=current_database()
 AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())), 'recovery fixture identity');
SELECT plumb_postings_demo.check((SELECT count(*)=210 FROM plumb_postings_concurrency.docs), 'recovery concurrent committed row count');
SELECT plumb_postings_demo.check(NOT EXISTS(SELECT FROM plumb_postings_concurrency.docs WHERE id BETWEEN 201 AND 220), 'recovery rolled-back rows absent');
SELECT plumb_postings_demo.compare('recovery concurrent all',
 'plumb_postings_concurrency.docs'::regclass,'docs_postings','*',
 'SELECT n AS id FROM generate_series(1,200) n UNION ALL SELECT n FROM generate_series(301,310) n');
SELECT plumb_postings_demo.compare('recovery concurrent beacon',
 'plumb_postings_concurrency.docs'::regclass,'docs_postings','beacon',
 'SELECT n AS id FROM generate_series(50,200,50) n UNION ALL SELECT n FROM generate_series(301,310) n');
SELECT plumb_postings_demo.compare('recovery concurrent abortedmarker',
 'plumb_postings_concurrency.docs'::regclass,'docs_postings','abortedmarker',
 'SELECT n AS id FROM generate_series(301,310) n');
SELECT plumb.index_stats('plumb_postings_concurrency.docs_postings'::regclass);
COMMIT;
\echo 'PASS: committed and aborted concurrency outcomes verified after recovery, without REINDEX.'
