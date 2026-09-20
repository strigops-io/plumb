#!/usr/bin/env python3
# Copyright (C) 2026 Plumb contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Local disposable PG17 scoring-cache freshness regression (stdlib + psql).

Requires the postings-demo-v1 marker in an owned plumb_postings_* database.
Creates ONLY the fixed NEW plumb_postings_freshness schema; refuses reuse.
No server management, installs, roles, GRANT, REINDEX, merges or fixture edits.
Leaves its committed data/index/identity for inspection. All outputs must be new
and outside this repository. Reader is the invoking database owner: no claim of
non-superuser or RLS coverage is made. Every read is transaction_read_only.

Example:
  python3 tests/scoring_freshness.py --host /absolute/local/socket --port 55441 \
    --database plumb_postings_checkpoint004 --summary /tmp/freshness.json \
    --log /tmp/freshness.log

The persistent READ COMMITTED reader repeats byte-identical SQL after committed
insert/update/delete operations in a distinct writer backend. Full-score ordered
CTIDs/float4 bits must equal a new backend, and top_k emitted order/bits must equal
the first K full-score rows. No txid_current() is called. Separate before/after
probes and in-statement probes use only txid_current_if_assigned(). A separate RR
reader anchors existing terms before updates and retains the exact old oracle.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import stat
import struct
import sys
import threading
import traceback
import uuid

sys.dont_write_bytecode = True
from postings_concurrency import Session, require, literal, BITMAP

SCHEMA = 'plumb_postings_freshness'
TABLE = SCHEMA + '.docs'
INDEX = SCHEMA + '.docs_default'
MARKER = 'scoring-freshness-v1'
K = 3

# A materialized rowset preserves the ranking context of the index scan.
FULL_SQL = f"""WITH ranked AS MATERIALIZED (
 SELECT ctid,plumb.full_score(ctid) AS score FROM {TABLE}
 WHERE body OPERATOR(pg_catalog.~~>) 'beacon'
)
SELECT json_build_object('rows',coalesce(json_agg(json_build_object(
 'ctid',ctid::text,'float4_bits',encode(float4send(score::real),'hex'))
 ORDER BY score DESC,ctid ASC),'[]'::json),
 'xid_in_statement',txid_current_if_assigned()::text) FROM ranked;"""
TOP_SQL = f"""SELECT json_build_object('rows',coalesce(json_agg(json_build_object(
 'ctid',q.ctid::text,'float4_bits',encode(float4send(q.score),'hex'))
 ORDER BY ord),'[]'::json),'xid_in_statement',txid_current_if_assigned()::text)
 FROM plumb.top_k('{INDEX}'::regclass,'beacon',{K})
 WITH ORDINALITY AS q(ctid,score,ord);"""
PROBE_SQL = """SELECT json_build_object('pid',pg_backend_pid(),
 'isolation',current_setting('transaction_isolation'),
 'read_only',current_setting('transaction_read_only'),
 'statement_timeout',current_setting('statement_timeout'),
 'xid',txid_current_if_assigned()::text);"""


class BoundedSession(Session):
    def finish(self, timeout=40):
        return super().finish(timeout)


def new_session(name, args, log, sessions, reader=False):
    session = BoundedSession(name, args, log, sessions)
    session.run("SET statement_timeout='30s'; SET lock_timeout='5s'; "
                "SET idle_in_transaction_session_timeout='120s'; "
                "SET standard_conforming_strings=on; "
                "SET default_transaction_isolation='read committed'; "
                "SET application_name='plumb-scoring-freshness'; " + BITMAP)
    if reader:
        session.run('SET default_transaction_read_only=on;')
    return session


def guard(database, run_id=None):
    own = '' if run_id is None else f"""
      IF (SELECT count(*) FROM {SCHEMA}.identity) <> 1 OR NOT EXISTS (
        SELECT FROM {SCHEMA}.identity WHERE marker='{MARKER}'
        AND run_id={literal(run_id)} AND database_name=current_database()
        AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())
        AND owner_name=current_user)
      OR NOT (SELECT nspowner=(SELECT oid FROM pg_roles WHERE rolname=current_user)
              FROM pg_namespace WHERE nspname='{SCHEMA}')
      THEN RAISE EXCEPTION 'Refusing: private identity/ownership mismatch'; END IF;
    """
    return f"""DO $guard$ BEGIN
      IF current_database() <> {literal(database)} OR inet_server_addr() IS NOT NULL
      OR NOT (SELECT pg_get_userbyid(datdba)=current_user FROM pg_database
              WHERE datname=current_database())
      OR (SELECT count(*) FROM plumb_postings_demo.identity) <> 1
      OR NOT EXISTS (SELECT FROM plumb_postings_demo.identity
        WHERE marker='postings-demo-v1' AND database_name=current_database()
        AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()))
      THEN RAISE EXCEPTION 'Refusing: local demo database identity/owner mismatch'; END IF;
      {own}
    END $guard$;"""


def measure(session, label, summary, isolation='read committed'):
    record = {'label': label, 'pid': session.pid, 'queries': {}}
    summary['reads'].append(record)
    for name, sql in [('full_score', FULL_SQL), ('top_k', TOP_SQL)]:
        before = session.scalar(PROBE_SQL)
        result = session.scalar(sql)
        after = session.scalar(PROBE_SQL)
        record['queries'][name] = {'before': before, 'result': result, 'after': after}
        for probe in (before, after):
            require(probe['pid'] == session.pid, 'Reader backend changed')
            require(probe['isolation'] == isolation and probe['read_only'] == 'on',
                    'Incorrect reader isolation/read-only setting')
            require(probe['statement_timeout'] == '30s', 'Incorrect statement timeout')
            require(probe['xid'] is None, 'Read assigned an XID')
        require(result['xid_in_statement'] is None, 'Scoring statement assigned an XID')
    full = record['queries']['full_score']['result']['rows']
    top = record['queries']['top_k']['result']['rows']
    require(full[:K] == top, label + ': exact top_k emitted order/float4 bits mismatch')
    record['exact_top_k_match'] = True
    return full


def corpus(session):
    return session.scalar(f"""SELECT json_build_object('n',count(*),
      'df',count(*) FILTER (WHERE body='beacon steady'),
      'all_two_tokens',bool_and(body IN ('beacon steady','copper steady')),
      'anchor',(SELECT json_build_object('ctid',ctid::text,'body',body)
                FROM {TABLE} WHERE id=1),
      'select_permission',has_table_privilege(current_user,'{TABLE}','SELECT'))
      FROM {TABLE};""")


def exercise(writer, args, log, sessions, summary):
    """Exercise only the newly-created, marker-verified fixture owned by this run."""
    run_id = summary['run_id']
    writer.run(guard(args.database, run_id))
    reader = new_session('persistent_rc', args, log, sessions, reader=True)
    rr = new_session('old_rr', args, log, sessions, reader=True)
    require(len({writer.pid, reader.pid, rr.pid}) == 3, 'Distinct backends required')
    summary['pids'] = {'writer': writer.pid, 'persistent_rc': reader.pid, 'old_rr': rr.pid}

    def stage(label, expected_n, expected_df):
        data = corpus(reader)
        require(data['n'] == expected_n and data['df'] == expected_df
                and data['all_two_tokens'] and data['select_permission'], 'Wrong corpus/permissions')
        observed = measure(reader, label + '_persistent', summary)
        repeated = measure(reader, label + '_persistent_repeat', summary)
        require(observed == repeated, 'Repeated SQL results changed without a write')
        fresh = new_session('fresh_' + label, args, log, sessions, reader=True)
        try:
            require(fresh.pid not in summary['pids'].values(), 'Oracle backend is not fresh')
            oracle = measure(fresh, label + '_fresh_oracle', summary)
            require(observed == oracle, label + ': persistent cache differs from fresh backend')
        finally:
            fresh.close()
            sessions.remove(fresh)
        require(len(observed) == expected_df, 'Wrong scored result cardinality')
        anchor = next(row for row in observed if row['ctid'] == data['anchor']['ctid'])
        item = {'label': label, 'corpus': data, 'oracle_pid': fresh.pid,
                'anchor_bits': anchor['float4_bits'],
                'anchor_score': struct.unpack('!f', bytes.fromhex(anchor['float4_bits']))[0],
                'exact_fresh_backend_match': True}
        summary['stages'].append(item)
        return observed

    def mutate(label, sql):
        writer.run('BEGIN; ' + guard(args.database, run_id) + sql + ' COMMIT;')
        summary['mutations'].append({'label': label, 'sql': sql, 'committed': True})

    stage('baseline', 12, 3)
    mutate('insert_nonmatching', f"INSERT INTO {TABLE} SELECT n,'copper steady' FROM generate_series(13,20) n;")
    old_oracle = stage('after_insert', 20, 3)
    rr.run('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
    summary['rr_snapshot'] = rr.scalar('SELECT to_json(pg_current_snapshot()::text);')
    anchored = measure(rr, 'rr_anchor_preexisting_beacon', summary, 'repeatable read')
    require(anchored == old_oracle, 'RR did not anchor the old oracle')
    mutate('update_nonmatching_to_beacon', f"UPDATE {TABLE} SET body='beacon steady' WHERE id BETWEEN 4 AND 7;")
    stage('after_update', 20, 7)
    require(measure(rr, 'rr_after_update_commit', summary, 'repeatable read') == old_oracle,
            'Old RR snapshot changed after committed UPDATE')
    mutate('delete_matching', f'DELETE FROM {TABLE} WHERE id BETWEEN 4 AND 7;')
    final = stage('after_delete', 16, 3)
    require(measure(rr, 'rr_after_delete_commit', summary, 'repeatable read') == old_oracle,
            'Old RR snapshot changed after committed DELETE')
    rr.run('COMMIT;')
    require(measure(rr, 'former_rr_now_fresh_rc', summary) == final,
            'Former RR backend failed to refresh after COMMIT')
    stages = summary['stages']
    require(all(s['corpus']['anchor'] == stages[0]['corpus']['anchor'] for s in stages),
            'Anchor CTID/body changed; not a corpus-only scoring change')
    scores = [s['anchor_score'] for s in stages]
    bits = [s['anchor_bits'] for s in stages]
    require(all(a != b for a, b in zip(bits, bits[1:])), 'Anchor bits failed to change on every mutation')
    require(scores[1] > scores[0] and scores[2] < scores[1] and scores[3] > scores[2],
            'IDF-driven score directions incorrect at fixed term frequency/document length')
    summary['idf_change_verified'] = True
    summary['rr_old_snapshot_exact_match'] = True
    summary['final_index_stats'] = writer.scalar(f"SELECT plumb.index_stats('{INDEX}'::regclass);")
    summary['script_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True, help='absolute local Unix socket directory')
    parser.add_argument('--port', required=True, type=int)
    parser.add_argument('--database', required=True)
    parser.add_argument('--summary', required=True)
    parser.add_argument('--log', required=True)
    args = parser.parse_args()
    require(args.database.startswith('plumb_postings_') and len(args.database) > 15,
            'Refusing database outside plumb_postings_ prefix')
    require(Path(args.host).is_absolute() and Path(args.host).is_dir(), 'Local socket path required')
    require(1 <= args.port <= 65535, 'Invalid port')
    require(stat.S_ISSOCK((Path(args.host) / ('.s.PGSQL.' + str(args.port))).stat().st_mode),
            'Expected PostgreSQL Unix socket')
    repo = Path(__file__).resolve().parents[1]
    outputs = [Path(args.summary).resolve(), Path(args.log).resolve()]
    require(outputs[0] != outputs[1], 'Output paths must differ')
    for output in outputs:
        require(not output.is_relative_to(repo), 'Evidence must be outside repository')
        require(output.parent.is_dir() and not output.exists(), 'Refusing existing/invalid output path')
    sessions, lock = [], threading.Lock()
    run_id = str(uuid.uuid4())
    summary = {'status': 'running', 'schema': SCHEMA, 'run_id': run_id,
               'database': args.database, 'host': args.host, 'port': args.port,
               'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
               'reads': [], 'stages': [], 'mutations': [],
               'sql': {'full_score': FULL_SQL, 'top_k': TOP_SQL},
               'method': 'Exact ordered CTIDs and float4send hex; no tolerance; read-only owner sessions; '
                         'new backend oracle per committed stage; no role/RLS coverage.'}
    exit_code = 1
    with open(args.summary, 'x', encoding='utf-8') as evidence, open(args.log, 'x', encoding='utf-8') as logfile:
        def log(message):
            with lock:
                logfile.write(datetime.datetime.now(datetime.timezone.utc).isoformat() + ' ' + message + '\n')
                logfile.flush()
        try:
            writer = new_session('writer', args, log, sessions)
            writer.run(guard(args.database))
            summary['environment'] = writer.scalar("""SELECT json_build_object(
              'version',version(),'server_version_num',current_setting('server_version_num')::int,
              'role',current_user,'superuser',(SELECT rolsuper FROM pg_roles WHERE rolname=current_user),
              'extension_version',(SELECT extversion FROM pg_extension WHERE extname='plumb'));
            """)
            require(170000 <= summary['environment']['server_version_num'] < 180000, 'PG17 required')
            writer.run(f"""BEGIN; {guard(args.database)}
              CREATE SCHEMA {SCHEMA};
              CREATE TABLE {SCHEMA}.identity(marker text NOT NULL,run_id text NOT NULL,
                database_name text NOT NULL,database_oid oid NOT NULL,owner_name text NOT NULL);
              INSERT INTO {SCHEMA}.identity SELECT '{MARKER}',{literal(run_id)},current_database(),
                oid,current_user FROM pg_database WHERE datname=current_database();
              CREATE TABLE {TABLE}(id integer PRIMARY KEY,body text NOT NULL)
                WITH(autovacuum_enabled=false);
              INSERT INTO {TABLE} SELECT n,CASE WHEN n<=3 THEN 'beacon steady'
                ELSE 'copper steady' END FROM generate_series(1,12) n;
              CREATE INDEX docs_default ON {TABLE} USING plumb(body);
              """)
            summary['initial_index_stats'] = writer.scalar(f"SELECT plumb.index_stats('{INDEX}'::regclass);")
            # index_stats.format_version describes the metapage, not term segments.
            summary['initial_term_stats'] = writer.scalar(
                f"SELECT plumb.term_stats('{INDEX}'::regclass,'beacon');")
            versions = summary['initial_term_stats']['term_versions']
            require(versions['v1'] == 0 and versions['v2'] > 0, 'Default term segments are not v2')
            writer.run('COMMIT;')
            summary['schema_created'] = True
            exercise(writer, args, log, sessions, summary)
            summary['status'] = 'passed'
            exit_code = 0
        except Exception as exc:
            summary.update(status='failed', error=str(exc), traceback=traceback.format_exc())
            log(summary['traceback'])
        finally:
            errors = []
            for session in reversed(sessions):
                try:
                    session.close()
                except Exception as exc:
                    errors.append(session.name + ': ' + str(exc))
            summary['cleanup_errors'] = errors
            if errors:
                summary['status'], exit_code = 'failed', 1
            summary['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            json.dump(summary, evidence, indent=2)
            evidence.write('\n')
    print(json.dumps({'status': summary['status'], 'summary': args.summary, 'log': args.log,
                      'error': summary.get('error')}))
    return exit_code


if __name__ == '__main__':
    sys.exit(main())
