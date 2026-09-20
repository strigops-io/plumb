-- Copyright (C) 2026 Plumb contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
-- Disposable permanent fixtures only; run after postings_demo.sql. COMMITS data.
\set ON_ERROR_STOP on
\if :{?disposable_db}
\else
  \quit 3
\endif
SELECT current_database()=:'disposable_db'
 AND current_database() LIKE 'plumb\_postings\_%' ESCAPE '\' AS safe_database \gset
\if :safe_database
\else
  \quit 3
\endif
SET search_path=pg_catalog,public;
SELECT plumb_postings_demo.check((SELECT count(*)=1 FROM plumb_postings_demo.identity
 WHERE marker='postings-demo-v1' AND database_name=current_database()
 AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())), 'edge fixture identity');
CREATE SCHEMA plumb_postings_edges; -- refuse reuse, never overwrite an existing fixture
CREATE TABLE plumb_postings_edges.hotdocs(id integer PRIMARY KEY, body text)
 WITH(fillfactor=30,autovacuum_enabled=false);
INSERT INTO plumb_postings_edges.hotdocs VALUES (1,'oldmarker'),(2,'unchanged');
UPDATE plumb_postings_edges.hotdocs SET body='beacon' WHERE id=1;
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT plumb_postings_demo.check((SELECT n_tup_hot_upd>0 FROM pg_stat_user_tables
 WHERE relid='plumb_postings_edges.hotdocs'::regclass), 'HOT update happened before index build');
CREATE INDEX hotdocs_postings ON plumb_postings_edges.hotdocs USING plumb(body) WITH(storage='postings_v1');
SELECT plumb_postings_demo.compare('prebuild HOT beacon',
 'plumb_postings_edges.hotdocs'::regclass,'hotdocs_postings','beacon','SELECT 1 AS id');
SELECT plumb_postings_demo.compare('prebuild HOT oldmarker',
 'plumb_postings_edges.hotdocs'::regclass,'hotdocs_postings','oldmarker','SELECT 1 AS id WHERE false');

CREATE TABLE plumb_postings_edges.partialdocs(id integer, body text, active boolean)
 WITH(autovacuum_enabled=false);
INSERT INTO plumb_postings_edges.partialdocs VALUES(1,'beacon',true);
CREATE TABLE plumb_postings_edges.oldtid AS SELECT ctid::text AS tid FROM plumb_postings_edges.partialdocs;
CREATE INDEX partialdocs_postings ON plumb_postings_edges.partialdocs USING plumb(body)
 WITH(storage='postings_v1') WHERE active;
DELETE FROM plumb_postings_edges.partialdocs;
VACUUM plumb_postings_edges.partialdocs;
INSERT INTO plumb_postings_edges.partialdocs VALUES(2,'beacon',false);
SELECT plumb_postings_demo.check((SELECT ctid::text=(SELECT tid FROM plumb_postings_edges.oldtid)
 FROM plumb_postings_edges.partialdocs), 'VACUUM recycled indexed CTID for row outside partial predicate');
SET enable_seqscan=off;
DO $$ DECLARE p jsonb; q text; n integer; BEGIN
 FOREACH q IN ARRAY ARRAY['beacon','bea*'] LOOP
  EXECUTE format('SELECT count(*) FROM plumb_postings_edges.partialdocs WHERE active AND body ~~> %L',q) INTO n;
  PERFORM plumb_postings_demo.check(n=0,'stale partial CTID rechecked: '||q);
  EXECUTE format('EXPLAIN (ANALYZE,FORMAT JSON) SELECT id FROM plumb_postings_edges.partialdocs WHERE active AND body ~~> %L',q) INTO p;
  PERFORM plumb_postings_demo.check(EXISTS(SELECT FROM plumb_postings_demo.plan_nodes(p) node
   WHERE node->>'Index Name'='partialdocs_postings'), 'partial recheck uses postings index: '||q);
  INSERT INTO plumb_postings_demo.evidence(label,detail) VALUES('partial CTID reuse: '||q,p);
 END LOOP;
END $$;
INSERT INTO plumb_postings_edges.partialdocs VALUES(3,'beacon',true);
SELECT plumb_postings_demo.check((SELECT array_agg(id ORDER BY id)=ARRAY[3]
 FROM plumb_postings_edges.partialdocs WHERE active AND body ~~> 'beacon'), 'partial reuse retains genuine new match');
SET enable_seqscan=on;
\echo 'PASS: prebuild HOT root and proven CTID reuse outside partial predicate.'
