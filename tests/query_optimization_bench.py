#!/usr/bin/env python3
# Copyright (C) 2026 Plumb contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Checkpoint005 local benchmark against the preserved checkpoint004 fixtures.

Python stdlib + psql only. Never install extensions, manage clusters, ANALYZE,
rewrite corpus rows, merge, drop, or replace objects. Each invocation creates a
NEW private plumb_postings_querybench_<uuid> schema containing only run identity.
Only --reindex authorizes ordinary REINDEX of the three fixed fixture indexes.
Run once WITHOUT --reindex to measure new code reading the original v1 segments,
then separately WITH --reindex to measure the same heap fixtures rebuilt by new
code. A reindex run records before/after stats and heap fingerprints, not another
full before-timing phase. Existing output files and paths inside the repo refused.

This is deliberately not a multi-session READ COMMITTED freshness test. Fresh
psql backends per measurement match the baseline profiler, not a persistent
application session. No cold-cache or ARM-runtime claim follows from this test.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import time
import traceback
import uuid

BASELINE = Path(__file__).resolve().parents[1] / 'docs/evidence/checkpoint-005-baseline.json'
FIXTURES = (
    ('growth', 'plumb_postings_growth.docs', 'plumb_postings_growth.docs_default', 200000),
    ('demo', 'plumb_postings_demo.docs', 'plumb_postings_demo.docs_postings', 30002),
    ('merge', 'plumb_postings_merge.docs', 'plumb_postings_merge.docs_default', 55),
)
QUERIES = ('beacon', 'absentmarker', 'beacon AND copper', 'beacon OR markerone')
ROUTES = {
    'index': 'SET enable_seqscan=off; SET enable_indexscan=off; '
             'SET enable_indexonlyscan=off; SET enable_bitmapscan=on;',
    'seq': 'SET enable_seqscan=on; SET enable_indexscan=off; '
           'SET enable_indexonlyscan=off; SET enable_bitmapscan=off;',
    'default': 'SET enable_seqscan=on; SET enable_indexscan=on; '
               'SET enable_indexonlyscan=on; SET enable_bitmapscan=on;',
}
METHOD = (
    'Three EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) samples per timed route; '
    'first sample included. Fresh psql/backend per operation; no parallel workers; '
    '60s statement timeout, 10s lock timeout, 75s subprocess deadline. '
    'Full sorted IDs checked once per route and against baseline. Synthetic fixed '
    'corpora; correctness/diagnostics warm caches before timings; no cache flush. '
    'Shared buffer reads are not proven physical disk reads; buffer hits are '
    'accesses, not distinct pages. SELECT can dirty hint bits. term_stats '
    'read_payload_bytes describes that inspection, not EXPLAIN query I/O. '
    'Physical counts include stale/dead/aborted/duplicate history, not visible df.'
)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def literal(value):
    # Every SQL connection sets standard_conforming_strings=on first.
    require('\x00' not in value, 'NUL in SQL literal')
    return "'" + value.replace("'", "''") + "'"


def identifier(value):
    return '"' + value.replace('"', '""') + '"'


def relation(value):
    return '.'.join(identifier(part) for part in value.split('.'))


def nodes(plan):
    yield plan
    for child in plan.get('Plans', []):
        yield from nodes(child)


def brief(plan):
    keys = ('Node Type', 'Relation Name', 'Index Name', 'Plan Rows', 'Actual Rows',
            'Actual Loops', 'Shared Hit Blocks', 'Shared Read Blocks',
            'Shared Dirtied Blocks', 'Shared Written Blocks', 'Local Hit Blocks',
            'Local Read Blocks', 'Temp Read Blocks', 'Temp Written Blocks',
            'Sort Method', 'Rows Removed by Index Recheck', 'Rows Removed by Filter')
    return {'execution_ms': plan['Execution Time'], 'planning_ms': plan['Planning Time'],
            'planning': plan.get('Planning', {}),
            'nodes': [{key: node[key] for key in keys if key in node}
                      for node in nodes(plan['Plan'])]}


def distribution(values):
    return {'median': statistics.median(values), 'min': min(values), 'max': max(values),
            'range': max(values) - min(values), 'samples': values}


class Database:
    def __init__(self, args):
        self.database = args.database
        self.command = ['psql', '-X', '-qAt', '-w', '-v', 'ON_ERROR_STOP=1',
                        '-h', str(args.hostsocket), '-p', str(args.port), '-d', args.database]
        self.env = os.environ.copy()
        # Do not inherit service/options that can redirect or alter this local run.
        for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGOPTIONS', 'PGHOSTADDR'):
            self.env.pop(key, None)
        self.env['PGCONNECT_TIMEOUT'] = '5'
        self.env['PGAPPNAME'] = 'plumb-checkpoint005-benchmark'

    def sql(self, statement, write=False):
        setup = ("SET statement_timeout='60s'; SET lock_timeout='10s'; "
                 "SET standard_conforming_strings=on; SET search_path=pg_catalog,public; "
                 "SET max_parallel_workers_per_gather=0; SET default_transaction_read_only=on;")
        # A separate -c ensures BEGIN READ WRITE can explicitly override a read-only
        # cluster default, only after caller-side guards and only for authorized DDL.
        transaction = 'BEGIN READ WRITE; ' if write else 'BEGIN READ ONLY; '
        if write:
            transaction += self.write_guard()
        proc = subprocess.run(self.command + ['-c', setup, '-c', transaction + statement + '; COMMIT;'],
                              capture_output=True, text=True, timeout=75, env=self.env)
        require(proc.returncode == 0, 'psql failed: ' + proc.stderr.strip())
        return proc.stdout.strip()

    def obj(self, statement):
        return json.loads(self.sql(statement))

    def write_guard(self):
        return """DO $guard$ BEGIN
          IF current_database() <> %s OR inet_server_addr() IS NOT NULL
             OR NOT (SELECT pg_get_userbyid(datdba)=current_user FROM pg_database
                     WHERE datname=current_database())
             OR (SELECT count(*) FROM plumb_postings_demo.identity) <> 1
             OR NOT EXISTS (SELECT FROM plumb_postings_demo.identity
                   WHERE marker='postings-demo-v1' AND database_name=current_database()
                     AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()))
          THEN RAISE EXCEPTION 'Refusing benchmark write: local owner/identity guard'; END IF;
        END $guard$; """ % literal(self.database)

    def plan(self, query, route, table=None, index=None):
        result = self.obj(ROUTES[route] + ' EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) ' + query)[0]
        if table is not None:
            plan_nodes = list(nodes(result['Plan']))
            if route == 'index':
                require(any(n['Node Type'] == 'Bitmap Index Scan'
                            and n.get('Index Name') == index.split('.')[-1] for n in plan_nodes),
                        'Forced index route did not use expected bitmap index: ' + index)
            if route == 'seq':
                require(any(n['Node Type'] == 'Seq Scan'
                            and n.get('Relation Name') == table.split('.')[-1] for n in plan_nodes),
                        'Missing sequential oracle plan: ' + table)
                require(not any('Index' in n['Node Type'] for n in plan_nodes),
                        'Sequential reference unexpectedly used an index')
        return result

    def samples(self, query, route, table=None, index=None):
        plans = [self.plan(query, route, table, index) for _ in range(3)]
        execution = distribution([p['Execution Time'] for p in plans])
        return {'plans': plans, 'summary': [brief(p) for p in plans],
                'execution_ms': execution,
                'planning_ms': distribution([p['Planning Time'] for p in plans]),
                'median_execution_ms': execution['median']}


def load_baseline():
    raw = BASELINE.read_bytes()
    baseline = json.loads(raw)
    require(not baseline.get('error'), 'Baseline recorded an error')
    require(baseline['repeats'] == 3, 'Unexpected baseline sample count')
    require(baseline['environment']['database'] == 'plumb_postings_checkpoint004',
            'Unexpected baseline database')
    expected = [(tag, table, index, count) for tag, table, index, count in FIXTURES]
    observed = [(f['name'], f['table'], f['index'], f['before']['rows'])
                for f in baseline['fixtures']]
    require(observed == expected, 'Baseline fixture names/counts differ from fixed allowlist')
    for fixture in baseline['fixtures']:
        require([q['query'] for q in fixture['queries']] == list(QUERIES),
                'Unexpected baseline query cases')
        for query in fixture['queries']:
            require(query['routes']['seq']['ids'] == query['routes']['index']['ids'],
                    'Baseline IDs disagree')
    return baseline, hashlib.sha256(raw).hexdigest()


def inspect_fixture(db, table, index):
    # Hash sorted row identity, CTID and text digest. Never materialize corpus text
    # into the log, and never modify/reload it. NULL is distinct from empty text.
    return db.obj("SELECT jsonb_build_object('rows',count(*),'heap_fingerprint',"
                  "md5(string_agg(id::text || ':' || ctid::text || ':' || "
                  "coalesce(md5(body),'<NULL>'),'|' ORDER BY id)),"
                  "'stats',plumb.index_stats(" + literal(index) + "::regclass),"
                  "'index_bytes',pg_relation_size(" + literal(index) + "::regclass),"
                  "'heap_bytes',pg_relation_size(" + literal(table) + "::regclass)) FROM "
                  + relation(table))


def term_stats(db, index, query):
    value = db.obj('SELECT plumb.term_stats(' + literal(index) + '::regclass,' + literal(query) + ')')
    require(all(key in value for key in ('physical_count_upper_bounds', 'term_versions',
                                        'read_payload_bytes', 'estimated_cardinality')),
            'Missing expected term_stats diagnostics')
    return value


def guard_fixture(db, table, index):
    result = db.obj("SELECT jsonb_build_object('safe', h.relkind='r' AND i.relkind='i' "
                    "AND h.relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user) "
                    "AND i.relowner=h.relowner AND am.amname='plumb' "
                    "AND x.indrelid=h.oid AND x.indisvalid AND x.indisready AND x.indislive "
                    "AND x.indnkeyatts=1 AND x.indnatts=1 AND x.indexprs IS NULL "
                    "AND x.indpred IS NULL AND pg_get_indexdef(i.oid,1,true)='body',"
                    "'table_oid',h.oid,'index_oid',i.oid,'index_definition',pg_get_indexdef(i.oid)) "
                    "FROM pg_class h CROSS JOIN pg_class i JOIN pg_index x ON x.indexrelid=i.oid "
                    "JOIN pg_am am ON am.oid=i.relam WHERE h.oid=" + literal(table) + "::regclass "
                    "AND i.oid=" + literal(index) + '::regclass')
    require(result['safe'] is True, 'Unsafe fixture ownership/index definition: ' + index)
    return result


def environment(db):
    return db.obj("SELECT jsonb_build_object('version',version(),'database',current_database(),"
                  "'local_socket',inet_server_addr() IS NULL,'owner',"
                  "(SELECT pg_get_userbyid(datdba)=current_user FROM pg_database WHERE datname=current_database()),"
                  "'extension',(SELECT jsonb_build_object('version',extversion,'schema',n.nspname) "
                  "FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE extname='plumb'),"
                  "'settings',(SELECT jsonb_object_agg(name,setting) FROM pg_settings WHERE name IN "
                  "('shared_buffers','work_mem','jit','fsync','full_page_writes','autovacuum',"
                  "'default_transaction_read_only','block_size','max_parallel_workers_per_gather',"
                  "'server_version','listen_addresses','port','statement_timeout','lock_timeout')),"
                  "'activity',(SELECT coalesce(jsonb_agg(jsonb_build_object('backend_type',backend_type,"
                  "'database',datname,'state',state,'application_name',application_name)),'[]'::jsonb) "
                  "FROM pg_stat_activity WHERE pid<>pg_backend_pid()))")


def benchmark_query(db, fixture, old_query, skip_seq):
    table, index, query = fixture['table'], fixture['index'], old_query['query']
    statement = 'SELECT id FROM ' + relation(table) + ' WHERE body OPERATOR(pg_catalog.~~>) ' + literal(query)
    result = {'query': query, 'sql': statement, 'routes': {},
              'term_stats': term_stats(db, index, query)}
    fixture['queries'].append(result)
    for route in ('index', 'seq'):
        ids = db.obj(ROUTES[route] + "SELECT coalesce(jsonb_agg(id ORDER BY id),'[]'::jsonb) FROM ("
                     + statement + ') q')
        record = {'ids': ids, 'count': len(ids), 'baseline_ids_match': ids == old_query['routes'][route]['ids']}
        result['routes'][route] = record
        require(record['baseline_ids_match'], 'IDs changed relative to checkpoint004: ' + table + ' ' + query)
        if route == 'seq' and skip_seq:
            # No ANALYZE execution here; still verify route and retain the plan estimate.
            plan = db.obj(ROUTES[route] + ' EXPLAIN (FORMAT JSON) ' + statement)[0]
            require(any(n['Node Type'] == 'Seq Scan' for n in nodes(plan['Plan'])),
                    'Skipped-timing sequential oracle must still plan a Seq Scan')
            record.update({'timings_skipped': True, 'estimated_plan': plan})
        else:
            record.update(db.samples(statement, route, table, index))
            previous = old_query['routes'][route]['median_execution_ms']
            current = record['median_execution_ms']
            record['baseline_median_execution_ms'] = previous
            record['baseline_over_current_ratio'] = previous / current if current else None
    result['exact_ids_match'] = result['routes']['index']['ids'] == result['routes']['seq']['ids']
    require(result['exact_ids_match'], 'Full ID comparison failed')
    result['default_plan'] = db.plan(statement, 'default')
    result['default_summary'] = brief(result['default_plan'])
    result['baseline_default_summary'] = old_query['default_summary']


def benchmark_topk(db, fixture, old_fixture):
    table, index = fixture['table'], fixture['index']
    reference = ('SELECT ctid,plumb.full_score(ctid) AS score FROM ' + relation(table)
                 + " WHERE body OPERATOR(pg_catalog.~~>) 'beacon' ORDER BY score DESC,ctid ASC LIMIT 10")
    function = 'SELECT * FROM plumb.top_k(' + literal(index) + "::regclass,'beacon',10)"
    result = {'reference_sql': reference, 'top_k_sql': function,
              'comparison': 'Ordered CTID text and encode(float4send(score),hex); no rounding',
              'baseline_topk_note': 'Old profiler used id and no CTID tie-break. Timing is contextual only; '
                                    'old rounded JSON scores are NOT an exact-ranking oracle.',
              'baseline_topk_median_execution_ms': old_fixture.get('topk', {}).get('median_execution_ms')}
    fixture['topk'] = result
    result['reference_results'] = db.obj(ROUTES['index'] +
        "SELECT coalesce(jsonb_agg(jsonb_build_object('ctid',ctid::text,'float4_bits',"
        "encode(float4send(score::real),'hex')) ORDER BY score DESC,ctid ASC),'[]'::jsonb) FROM ("
        + reference + ') q')
    # WITH ORDINALITY checks emitted order, rather than sorting away a top_k bug.
    result['top_k_results'] = db.obj(
        "SELECT coalesce(jsonb_agg(jsonb_build_object('ctid',q.ctid::text,'float4_bits',"
        "encode(float4send(q.score),'hex')) ORDER BY ord),'[]'::jsonb) FROM plumb.top_k("
        + literal(index) + "::regclass,'beacon',10) WITH ORDINALITY AS q(ctid,score,ord)")
    result['exact_ctids_float4_bits_match'] = result['reference_results'] == result['top_k_results']
    require(len(result['reference_results']) == 10, 'Expected exactly ten demo top-k rows')
    require(result['exact_ctids_float4_bits_match'], 'Top-k CTIDs, order or float4 bits differ')
    result['reference'] = db.samples(reference, 'index', table, index)
    result['top_k'] = db.samples(function, 'default')
    current = result['top_k']['median_execution_ms']
    result['reference_over_top_k_ratio'] = result['reference']['median_execution_ms'] / current if current else None


def run(args, output):
    baseline, checksum = load_baseline()
    require(args.database == baseline['environment']['database'], 'Database must match known baseline identity')
    output.update({'baseline_path': str(BASELINE), 'baseline_sha256': checksum,
                   'baseline_library_sha256': baseline.get('library_sha256'),
                   'baseline_environment': baseline['environment'], 'fixtures': [], 'reindex': []})
    db = Database(args)
    env = environment(db)
    output['environment'] = env
    require(env['local_socket'] and env['owner'], 'Local socket and database owner required')
    require(env['extension'] and env['extension']['schema'] == 'plumb', 'Expected installed plumb extension')
    identity = db.obj("SELECT jsonb_build_object('rows',count(*),'matches',count(*) FILTER "
                      "(WHERE marker='postings-demo-v1' AND database_name=current_database() "
                      "AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()))) "
                      "FROM plumb_postings_demo.identity")
    require(identity == {'rows': 1, 'matches': 1}, 'Known demo identity marker does not match')
    require(db.obj("SELECT to_jsonb(pg_get_userbyid(relowner)=current_user) FROM pg_class "
                   "WHERE oid='plumb_postings_demo.identity'::regclass") is True, 'Identity table not owned by caller')
    require(db.obj("SELECT to_jsonb(to_regprocedure('plumb.term_stats(regclass,text)') IS NOT NULL "
                   "AND to_regprocedure('plumb.top_k(regclass,text,integer)') IS NOT NULL)"),
            'Install/register the intended checkpoint005 APIs before benchmarking; harness never installs')
    for tag, table, index, count in FIXTURES:
        fixture = {'name': tag, 'table': table, 'index': index,
                   'identity': guard_fixture(db, table, index),
                   'before': inspect_fixture(db, table, index), 'queries': []}
        output['fixtures'].append(fixture)
        require(fixture['before']['rows'] == count, 'Fixture row count differs from baseline: ' + table)

    schema = 'plumb_postings_querybench_' + uuid.uuid4().hex
    output['scratch_schema'] = schema
    # No IF NOT EXISTS: even a UUID collision must fail, never reuse an object.
    db.sql('CREATE SCHEMA ' + identifier(schema) + '; CREATE TABLE ' + identifier(schema)
           + '.identity(marker text NOT NULL,label text NOT NULL,database_oid oid NOT NULL); INSERT INTO '
           + identifier(schema) + '.identity SELECT ' + literal('checkpoint005-querybench-v1') + ','
           + literal(args.label) + ',oid FROM pg_database WHERE datname=current_database()', write=True)
    output['scratch_schema_created'] = True

    if args.reindex:
        for fixture in output['fixtures']:
            table, index = fixture['table'], fixture['index']
            guard_fixture(db, table, index)
            event = {'index': index, 'before': inspect_fixture(db, table, index),
                     'before_term_stats': {q: term_stats(db, index, q) for q in QUERIES}}
            output['reindex'].append(event)
            # Recheck index->heap binding under a SHARE heap lock. Ordinary REINDEX
            # only; no CONCURRENTLY, schema/database-wide rebuild, or corpus rewrite.
            checked = ("DO $binding$ BEGIN IF NOT EXISTS (SELECT FROM pg_index x JOIN pg_class c "
                       "ON c.oid=x.indexrelid JOIN pg_am a ON a.oid=c.relam WHERE x.indexrelid="
                       + literal(index) + '::regclass AND x.indrelid=' + literal(table)
                       + "::regclass AND c.relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user) "
                       "AND a.amname='plumb' AND x.indisvalid AND x.indisready AND x.indislive) "
                       "THEN RAISE EXCEPTION 'Reindex binding changed'; END IF; END $binding$; ")
            started = time.monotonic()
            db.sql('LOCK TABLE ' + relation(table) + ' IN SHARE MODE; ' + checked
                   + 'REINDEX INDEX ' + relation(index), write=True)
            event['elapsed_seconds'] = time.monotonic() - started
            event['after'] = inspect_fixture(db, table, index)
            event['after_term_stats'] = {q: term_stats(db, index, q) for q in QUERIES}
            require(event['before']['heap_fingerprint'] == event['after']['heap_fingerprint'],
                    'Heap corpus/CTIDs changed across REINDEX')

    for fixture, old in zip(output['fixtures'], baseline['fixtures']):
        fixture['measurement_before'] = inspect_fixture(db, fixture['table'], fixture['index'])
        for query in old['queries']:
            benchmark_query(db, fixture, query, args.skip_sequential_timings)
        if fixture['name'] == 'demo':
            benchmark_topk(db, fixture, old)
        fixture['after'] = inspect_fixture(db, fixture['table'], fixture['index'])
        require(fixture['before']['heap_fingerprint'] == fixture['after']['heap_fingerprint'],
                'Heap fixture changed during benchmark: ' + fixture['table'])
        require(fixture['measurement_before']['stats'] == fixture['after']['stats'],
                'Index stats changed during measurement: ' + fixture['index'])
    output['final_environment'] = environment(db)
    output['status'] = 'passed'


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--hostsocket', type=Path, required=True, help='Existing local Unix socket directory')
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--database', required=True, help='Must match plumb_postings_checkpoint004 baseline')
    parser.add_argument('--label', required=True, help='Unique phase label, e.g. newcode-v1 or rebuilt-v2')
    parser.add_argument('--output', type=Path, help='New JSON outside repo; default checkpoint005-bench-LABEL.json beside the repository')
    parser.add_argument('--reindex', action='store_true', help='Explicitly rebuild ONLY the three known fixture indexes before timing')
    parser.add_argument('--skip-sequential-timings', action='store_true',
                        help='Keep full sequential ID oracle but omit its three ANALYZE timings (not baseline-complete)')
    args = parser.parse_args()
    require(args.hostsocket.is_absolute() and args.hostsocket.is_dir(), 'Existing absolute socket directory required')
    require(1 <= args.port <= 65535, 'Invalid port')
    require(args.database.startswith('plumb_postings_'), 'Refusing database without plumb_postings_ prefix')
    require(re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,79}', args.label), 'Invalid phase label')
    path = (args.output or Path(__file__).resolve().parents[2] / ('checkpoint005-bench-' + args.label + '.json')).resolve()
    repo = Path(__file__).resolve().parents[1]
    require(repo != path and repo not in path.parents, 'JSON output must be outside the repository')
    require(path.parent.is_dir(), 'Output parent must already exist')
    output = {'label': args.label, 'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'status': 'running', 'method': METHOD, 'repeats': 3,
              'reindex_authorized': args.reindex, 'sequential_timings_skipped': args.skip_sequential_timings,
              'connection': {'hostsocket': str(args.hostsocket), 'port': args.port, 'database': args.database},
              'freshness_test': 'Not covered: separate persistent-session READ COMMITTED test required',
              'arm_runtime': 'Not tested by this harness author; see server version for actual run architecture'}
    started = time.monotonic()
    # Refuse overwrite and reserve log before any database side effects. A failure
    # still writes partial evidence, including any already completed REINDEX.
    with path.open('x', encoding='utf-8') as stream:
        try:
            run(args, output)
        except Exception as error:
            output.update({'status': 'failed', 'error': str(error), 'traceback': traceback.format_exc()})
        finally:
            output['elapsed_seconds'] = time.monotonic() - started
            json.dump(output, stream, indent=2, allow_nan=False)
            stream.write('\n')
    print(str(path), file=sys.stderr)
    return 0 if output['status'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
