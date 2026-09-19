-- Copyright (C) 2026 Plumb contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
-- DESTRUCTIVE TEST: disposable, empty database ONLY. Never point at application data.
-- Requires separately installed, unchanged public PlanetScale Lead (tin) and Plumb.
-- This does not test hosted/proprietary TIN. No tin aliases are supplied by Plumb.
-- Run with psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_coexist_NAME
--      -d plumb_coexist_NAME -f tests/coexistence.sql
-- All test objects and extension drops roll back, including on disconnect/error.
\set ON_ERROR_STOP on
\if :{?disposable_db}
\else
  \echo 'Refusing: pass -v disposable_db=plumb_coexist_NAME for a NEW empty database.'
  \quit 3
\endif
SELECT current_database() = :'disposable_db'
   AND current_database() LIKE 'plumb\_coexist\_%' ESCAPE '\' AS safe_database \gset
\if :safe_database
\else
  \echo 'Refusing: database must match disposable_db and start with plumb_coexist_.'
  \quit 3
\endif

BEGIN;
SET LOCAL search_path=pg_catalog,public;
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_extension WHERE extname <> 'plpgsql')
     OR EXISTS (SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                WHERE n.nspname NOT IN ('pg_catalog','information_schema')
                  AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%')
     OR EXISTS (SELECT FROM pg_namespace WHERE nspname IN ('tin','plumb','plumb_coexist','plumb_shadow'))
  THEN RAISE EXCEPTION 'Refusing: test requires an empty disposable database (only plpgsql allowed)'; END IF;
END $$;

CREATE TEMP TABLE checks(label text);
CREATE FUNCTION pg_temp.check(ok boolean, label text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  IF ok IS DISTINCT FROM true THEN RAISE EXCEPTION 'check failed: %', label; END IF;
  INSERT INTO pg_temp.checks VALUES(label);
END $$;
CREATE FUNCTION pg_temp.reject(sql text, expected text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  BEGIN EXECUTE sql;
  EXCEPTION WHEN OTHERS THEN
    IF SQLERRM <> expected THEN RAISE EXCEPTION 'unexpected error: %; expected: %', SQLERRM, expected; END IF;
    INSERT INTO pg_temp.checks VALUES('reject: ' || sql);
    RETURN;
  END;
  RAISE EXCEPTION 'expected rejection, query succeeded: %', sql;
END $$;
CREATE FUNCTION pg_temp.uses_index(sql text, index_name text, am_name text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE plan jsonb; found boolean;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || sql INTO plan;
  WITH RECURSIVE nodes(node) AS (
    SELECT plan->0->'Plan'
    UNION ALL SELECT child FROM nodes CROSS JOIN LATERAL jsonb_array_elements(node->'Plans') child
  ) SELECT EXISTS (SELECT FROM nodes WHERE node->>'Index Name'=index_name) INTO found;
  PERFORM pg_temp.check(found, 'EXPLAIN uses ' || index_name);
  PERFORM pg_temp.check((SELECT a.amname=am_name FROM pg_class c JOIN pg_am a ON a.oid=c.relam
                        WHERE c.oid=('plumb_coexist.' || index_name)::regclass), 'index AM: ' || index_name);
END $$;

-- Refuse preexisting identities rather than replacing/adopting them.
CREATE FUNCTION pg_temp.refuse_install(label text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  BEGIN EXECUTE 'CREATE EXTENSION plumb';
  EXCEPTION WHEN duplicate_function OR duplicate_object THEN
    INSERT INTO pg_temp.checks VALUES(label);
    RETURN;
  END;
  RAISE EXCEPTION 'expected duplicate-object rejection, installation succeeded: %', label;
END $$;
CREATE SCHEMA plumb;
CREATE FUNCTION plumb.amhandler(internal) RETURNS index_am_handler
  LANGUAGE internal AS 'bthandler';
SELECT pg_temp.refuse_install('preexisting handler blocks install');
SELECT pg_temp.check((SELECT prosrc='bthandler' FROM pg_proc
  WHERE oid='plumb.amhandler(internal)'::regprocedure)
  AND NOT EXISTS (SELECT FROM pg_extension WHERE extname='plumb'),
  'preexisting handler preserved and extension not installed');
DROP FUNCTION plumb.amhandler(internal);
DROP SCHEMA plumb;
CREATE OPERATOR pg_catalog.~~> (PROCEDURE=pg_catalog.texteq, LEFTARG=text, RIGHTARG=text);
SELECT pg_temp.refuse_install('preexisting operator blocks install');
SELECT pg_temp.check((SELECT oprcode='pg_catalog.texteq(text,text)'::regprocedure
  FROM pg_operator WHERE oid='pg_catalog.~~>(text,text)'::regoperator)
  AND NOT EXISTS (SELECT FROM pg_extension WHERE extname='plumb'),
  'preexisting operator preserved and extension not installed');
DROP OPERATOR pg_catalog.~~>(text,text);

-- Install Lead first, snapshot its objects, then ensure Plumb does not replace them.
CREATE EXTENSION tin;
CREATE TEMP TABLE lead_catalog_before AS
  SELECT p.oid, pg_get_functiondef(p.oid) AS definition
  FROM pg_proc p WHERE p.pronamespace='tin'::regnamespace;
CREATE EXTENSION plumb;
SELECT pg_temp.check(NOT EXISTS (
  SELECT FROM lead_catalog_before b LEFT JOIN pg_proc p ON p.oid=b.oid
  WHERE p.oid IS NULL OR pg_get_functiondef(p.oid) <> b.definition
), 'Plumb preserves Lead procedure OIDs and definitions');
SELECT pg_temp.check((SELECT count(*)=2 FROM pg_extension
  WHERE (extname='tin' AND extnamespace='tin'::regnamespace)
     OR (extname='plumb' AND extnamespace='plumb'::regnamespace AND extversion='0.1.0')),
  'independent extensions and schemas');

-- Exact signatures, namespace, library paths and extension membership.
SELECT pg_temp.check((SELECT oprcode='plumb.plumb_text_cmpfunc(text,text)'::regprocedure
  FROM pg_operator WHERE oid='pg_catalog.~~>(text,text)'::regoperator), 'Plumb operator procedure');
SELECT pg_temp.check((SELECT oprcode='tin.tin_text_cmpfunc(text,text)'::regprocedure
  FROM pg_operator WHERE oid='pg_catalog.==>(text,text)'::regoperator), 'Lead operator procedure');
SELECT pg_temp.check((SELECT count(*)=2 FROM pg_proc
  WHERE (oid='plumb.plumb_text_cmpfunc(text,text)'::regprocedure AND probin='plumb')
     OR (oid='tin.tin_text_cmpfunc(text,text)'::regprocedure AND probin='tin')), 'independent libraries');
WITH expected(classid,objid,owner) AS (VALUES
 ('pg_operator'::regclass, 'pg_catalog.~~>(text,text)'::regoperator::oid, 'plumb'),
 ('pg_operator'::regclass, 'pg_catalog.==>(text,text)'::regoperator::oid, 'tin'),
 ('pg_proc'::regclass, 'plumb.plumb_text_cmpfunc(text,text)'::regprocedure::oid, 'plumb'),
 ('pg_proc'::regclass, 'tin.tin_text_cmpfunc(text,text)'::regprocedure::oid, 'tin'),
 ('pg_proc'::regclass, 'plumb.score_support(internal)'::regprocedure::oid, 'plumb'),
 ('pg_proc'::regclass, 'tin.score_support(internal)'::regprocedure::oid, 'tin'),
 ('pg_proc'::regclass, 'plumb.highlight_support(internal)'::regprocedure::oid, 'plumb'),
 ('pg_proc'::regclass, 'tin.highlight_support(internal)'::regprocedure::oid, 'tin'),
 ('pg_am'::regclass, (SELECT oid FROM pg_am WHERE amname='plumb'), 'plumb'),
 ('pg_am'::regclass, (SELECT oid FROM pg_am WHERE amname='tin'), 'tin'),
 ('pg_opclass'::regclass, (SELECT oid FROM pg_opclass WHERE opcname='plumb_text_ops' AND opcnamespace='plumb'::regnamespace), 'plumb'),
 ('pg_opclass'::regclass, (SELECT oid FROM pg_opclass WHERE opcname='tin_text_ops' AND opcnamespace='tin'::regnamespace), 'tin')
) SELECT pg_temp.check(EXISTS (
 SELECT FROM pg_depend d JOIN pg_extension e ON e.oid=d.refobjid
 WHERE d.classid=x.classid AND d.objid=x.objid AND d.refclassid='pg_extension'::regclass
   AND d.deptype='e' AND e.extname=x.owner
), format('extension ownership %s %s -> %s', classid, objid, owner)) FROM expected x;
SELECT pg_temp.check((SELECT count(*)=2 FROM pg_am
 WHERE (amname='plumb' AND amhandler='plumb.amhandler(internal)'::regprocedure)
    OR (amname='tin' AND amhandler='tin.amhandler(internal)'::regprocedure)), 'AM handlers are independent');

CREATE SCHEMA plumb_coexist;
CREATE TABLE plumb_coexist.docs(id integer PRIMARY KEY, body text);
INSERT INTO plumb_coexist.docs VALUES (1,'beer wine'), (2,'beer beer wine'), (3,'wine');
INSERT INTO plumb_coexist.docs SELECT n,'noise' FROM generate_series(4,30) n;
CREATE INDEX docs_tin ON plumb_coexist.docs USING tin(body);
CREATE INDEX docs_plumb ON plumb_coexist.docs USING plumb(body);
ANALYZE plumb_coexist.docs;
SET LOCAL enable_seqscan=off;
SELECT pg_temp.uses_index('SELECT id FROM plumb_coexist.docs WHERE body ==> ''beer''', 'docs_tin', 'tin');
SELECT pg_temp.uses_index('SELECT id FROM plumb_coexist.docs WHERE body ~~> ''beer''', 'docs_plumb', 'plumb');
SELECT pg_temp.check((SELECT array_agg(id ORDER BY id)=ARRAY[1,2] FROM plumb_coexist.docs WHERE body ~~> 'beer'), 'Plumb matching rows');
SELECT pg_temp.check((SELECT array_agg(id ORDER BY id)=ARRAY[1,2,3] FROM plumb_coexist.docs WHERE body ==> 'wine'), 'Lead matching rows');

CREATE TEMP TABLE plumb_baseline AS SELECT id, plumb.score(ctid,1.0) AS score,
  plumb.full_score(ctid) AS full_score, plumb.max_score(ctid) AS max_score,
  plumb.highlight(body) AS highlight, plumb.highlight_ansi(body) AS ansi
  FROM plumb_coexist.docs WHERE body ~~> 'beer';
CREATE TEMP TABLE tin_baseline AS SELECT id, tin.score(ctid,1.0) AS score,
  tin.full_score(ctid) AS full_score, tin.max_score(ctid) AS max_score,
  tin.highlight(body) AS highlight, tin.highlight_ansi(body) AS ansi
  FROM plumb_coexist.docs WHERE body ==> 'wine';
SELECT pg_temp.check((SELECT bool_and(score>0 AND full_score>0 AND max_score>=full_score) FROM plumb_baseline), 'Plumb separate positive scores');
SELECT pg_temp.check((SELECT bool_and(score>0 AND full_score>0 AND max_score>=full_score) FROM tin_baseline), 'Lead separate positive scores');
SELECT pg_temp.check((SELECT array_agg(id ORDER BY full_score DESC)=ARRAY[2,1] FROM plumb_baseline), 'Plumb separate ranking');
SELECT pg_temp.check((SELECT highlight='<b>beer</b> wine' FROM plumb_baseline WHERE id=1), 'Plumb separate highlight');
SELECT pg_temp.check((SELECT highlight='beer <b>wine</b>' FROM tin_baseline WHERE id=1), 'Lead separate highlight');
CREATE TEMP TABLE mixed AS SELECT id,
  plumb.score(ctid,1.0) AS ps, plumb.full_score(ctid) AS pf, plumb.max_score(ctid) AS pm,
  tin.score(ctid,1.0) AS ts, tin.full_score(ctid) AS tf, tin.max_score(ctid) AS tm,
  plumb.highlight(body) AS ph, tin.highlight(body) AS th,
  plumb.highlight_ansi(body) AS pa, tin.highlight_ansi(body) AS ta
  FROM plumb_coexist.docs WHERE body ~~> 'beer' AND body ==> 'wine';
SELECT pg_temp.check((SELECT count(*)=2 AND bool_and(
  m.ps=p.score AND m.pf=p.full_score AND m.pm=p.max_score AND m.ph=p.highlight AND m.pa=p.ansi
  AND m.ts=t.score AND m.tf=t.full_score AND m.tm=t.max_score AND m.th=t.highlight AND m.ta=t.ansi)
  FROM mixed m JOIN plumb_baseline p USING(id) JOIN tin_baseline t USING(id)), 'mixed provider scores and highlights equal separate baselines');

-- Matching index presence must not make a foreign provider's operator bind.
SELECT pg_temp.reject('SELECT plumb.full_score(ctid) FROM plumb_coexist.docs WHERE body ==> ''beer''',
 'plumb.full_score() requires a plumb index scan and cannot be used in this query context');
SELECT pg_temp.reject('SELECT tin.full_score(ctid) FROM plumb_coexist.docs WHERE body ~~> ''beer''',
 'tin.full_score() requires a tin index scan and cannot be used in this query context');
SELECT pg_temp.reject('SELECT plumb.highlight(body) FROM plumb_coexist.docs WHERE body ==> ''beer''',
 'plumb.highlight() requires an explicit query or a matching plumb index scan');
SELECT pg_temp.reject('SELECT tin.highlight(body) FROM plumb_coexist.docs WHERE body ~~> ''beer''',
 'tin.highlight() requires an explicit query or a matching tin index scan');
SELECT pg_temp.reject('SELECT * FROM plumb.score_inspect(''plumb_coexist.docs_tin'', ''beer'')',
 'plumb.score_inspect() requires a plumb index');
SELECT pg_temp.reject('SELECT * FROM tin.score_inspect(''plumb_coexist.docs_plumb'', ''beer'')',
 'tin.score_inspect() requires a tin index');

-- Deliberately shadow ~~> and score_bound. Qualified lookup must ignore both.
CREATE SCHEMA plumb_shadow;
CREATE FUNCTION plumb_shadow.match(text,text) RETURNS boolean LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN true; END $$;
CREATE OPERATOR plumb_shadow.~~> (PROCEDURE=plumb_shadow.match, LEFTARG=text, RIGHTARG=text);
CREATE FUNCTION plumb_shadow.score_bound(text,text,int,int,int,real,real,real,text[],text[]) RETURNS real
 LANGUAGE plpgsql VOLATILE AS $$ BEGIN RAISE EXCEPTION 'shadow score_bound invoked'; END $$;
SET LOCAL search_path=plumb_shadow,pg_catalog,public;
SELECT pg_temp.reject('SELECT plumb.full_score(ctid) FROM plumb_coexist.docs WHERE body ~~> ''beer''',
 'plumb.full_score() requires a plumb index scan and cannot be used in this query context');
SELECT pg_temp.reject('SELECT plumb.highlight(body) FROM plumb_coexist.docs WHERE body ~~> ''beer''',
 'plumb.highlight() requires an explicit query or a matching plumb index scan');
SELECT pg_temp.check((SELECT array_agg(plumb.highlight(body) ORDER BY id)=ARRAY['<b>beer</b> wine','<b>beer</b> <b>beer</b> wine']
 FROM plumb_coexist.docs WHERE body OPERATOR(pg_catalog.~~>) 'beer' AND body ~~> 'wine'), 'shadow operator not merged into canonical highlights');
CREATE TEMP TABLE shadow_scores AS SELECT id, plumb.full_score(ctid) AS score
 FROM plumb_coexist.docs WHERE body OPERATOR(pg_catalog.~~>) 'beer' AND body ~~> 'wine';
SELECT pg_temp.check((SELECT count(*)=2 AND bool_and(s.score=p.full_score)
 FROM shadow_scores s JOIN plumb_baseline p USING(id)), 'shadow operator and function cannot contaminate scoring');
SET LOCAL search_path=pg_catalog,public;

-- Reject a Plumb query when only the Lead index exists, then restore Plumb's index.
DROP INDEX plumb_coexist.docs_plumb;
SELECT pg_temp.reject('SELECT plumb.full_score(ctid) FROM plumb_coexist.docs WHERE body ~~> ''beer''',
 'plumb.full_score() requires a plumb index scan and cannot be used in this query context');
SELECT pg_temp.reject('SELECT plumb.highlight(body) FROM plumb_coexist.docs WHERE body ~~> ''beer''',
 'plumb.highlight() requires an explicit query or a matching plumb index scan');
CREATE INDEX docs_plumb ON plumb_coexist.docs USING plumb(body);

-- Drop Lead first. Only its index should cascade; Plumb keeps working.
DROP EXTENSION tin CASCADE;
SELECT pg_temp.check(to_regclass('plumb_coexist.docs_tin') IS NULL
 AND to_regoperator('pg_catalog.==>(text,text)') IS NULL
 AND to_regclass('plumb_coexist.docs_plumb') IS NOT NULL, 'dropping Lead preserves Plumb objects');
SELECT pg_temp.uses_index('SELECT id FROM plumb_coexist.docs WHERE body ~~> ''beer''', 'docs_plumb', 'plumb');
SELECT pg_temp.check((SELECT count(*)=2 AND bool_and(plumb.full_score(ctid)>0 AND plumb.highlight(body) LIKE '%<b>beer</b>%')
 FROM plumb_coexist.docs WHERE body ~~> 'beer'), 'Plumb scores and highlights after dropping Lead');

-- Reinstall Lead while Plumb is present: exercise the reverse installation order.
CREATE EXTENSION tin;
CREATE INDEX docs_tin ON plumb_coexist.docs USING tin(body);
DROP EXTENSION plumb CASCADE;
SELECT pg_temp.check(to_regclass('plumb_coexist.docs_plumb') IS NULL
 AND to_regoperator('pg_catalog.~~>(text,text)') IS NULL
 AND to_regclass('plumb_coexist.docs_tin') IS NOT NULL, 'dropping Plumb preserves Lead objects');
SELECT pg_temp.uses_index('SELECT id FROM plumb_coexist.docs WHERE body ==> ''wine''', 'docs_tin', 'tin');
SELECT pg_temp.check((SELECT count(*)=3 AND bool_and(tin.full_score(ctid)>0 AND tin.highlight(body) LIKE '%<b>wine</b>%')
 FROM plumb_coexist.docs WHERE body ==> 'wine'), 'Lead scores and highlights after dropping Plumb');

SELECT count(*) AS passed_checks FROM pg_temp.checks;
ROLLBACK;
\echo 'PASS: public Lead and Plumb coexistence; all test changes rolled back.'
