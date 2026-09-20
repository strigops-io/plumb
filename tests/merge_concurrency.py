#!/usr/bin/env python3
# Copyright (C) 2026 Plumb contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Checkpoint004 local-only merge/MVCC/append/DDL/recovery harness.

Requires postings_demo.sql in plumb_postings_checkpoint004. Creates ONLY a NEW
plumb_postings_merge schema. Never manages a server, drops objects, creates roles,
or rebuilds an existing fixture. --verify-only rechecks saved committed fixtures
and stats after restart without merging/rebuilding/mutating anything.
Uses the adjacent postings_concurrency.Session psql helper and Python stdlib.
"""
import argparse
import datetime
import json
from pathlib import Path
import stat
import sys
import threading
import time
import traceback
import uuid

sys.dont_write_bytecode = True
from postings_concurrency import Session, require, literal, nodes, SEQUENTIAL, BITMAP

SCHEMA = 'plumb_postings_merge'
TABLE = SCHEMA + '.docs'
INDEX = 'docs_default'
INDEX_REL = SCHEMA + '.' + INDEX
DDL_TABLE = SCHEMA + '.ddl_docs'
DDL_INDEX = SCHEMA + '.ddl_default'
FINAL = {
    'oldterm': list(range(16, 41)),
    'newterm': list(range(1, 11)) + list(range(101, 111)),
    'committedmarker': list(range(201, 206)),
    'abortedmarker': [],
    'continuedmarker': list(range(401, 406)),
    'sharedterm': list(range(1, 11)) + list(range(16, 41)) + list(range(101, 111))
                  + list(range(201, 206)) + list(range(401, 406)),
}


def stats(session, index=INDEX_REL):
    return session.scalar(f'SELECT plumb.index_stats({literal(index)}::regclass);')


def compare(session, label, query, expected, summary, table=TABLE, index=INDEX):
    stmt = f'SELECT id FROM {table} WHERE body OPERATOR(pg_catalog.~~>) {literal(query)}'
    record = {'label': label, 'query': query, 'expected_ids': expected}
    for route, settings in [('seq', SEQUENTIAL), ('bitmap', BITMAP)]:
        session.run(settings)
        started = time.monotonic()
        plan = session.scalar('EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) ' + stmt + ';')
        ids = session.scalar("SELECT coalesce(json_agg(id ORDER BY id),'[]'::json) FROM (" + stmt + ') q;')
        record[route] = {'ids': ids, 'count': len(ids), 'plan': plan,
                         'wall_seconds': time.monotonic() - started}
        require(ids == expected, label + ': wrong ' + route + ' IDs')
        if route == 'seq':
            require(any(n['Node Type'] == 'Seq Scan' and n.get('Relation Name') == table.split('.')[-1]
                        for n in nodes(plan)), label + ': no sequential reference')
        else:
            require(any(n['Node Type'] == 'Bitmap Index Scan' and n.get('Index Name') == index
                        for n in nodes(plan)), label + ': wrong bitmap route')
    summary['queries'].append(record)


def activities(ctl, sessions):
    ctl.run('SELECT pg_stat_clear_snapshot();')
    pids = ','.join(str(s.pid) for s in sessions)
    return ctl.scalar("SELECT coalesce(json_agg(json_build_object('pid',pid,'state',state,"
                      "'xid',backend_xid::text,'wait_event_type',wait_event_type,'wait_event',wait_event,"
                      "'query',query,'xact_start',xact_start,'query_start',query_start)), '[]'::json) "
                      f'FROM pg_stat_activity WHERE pid IN ({pids});')


def wait_for(ctl, sessions, predicate, label):
    deadline = time.monotonic() + 15
    while True:
        evidence = activities(ctl, sessions)
        if len(evidence) == len(sessions) and predicate(evidence):
            return evidence
        require(time.monotonic() < deadline, label + ': did not observe required overlap/wait')
        time.sleep(0.05)


def merge(session):
    result = session.scalar(f'SELECT plumb.merge_index({literal(INDEX_REL)}::regclass);')
    require(result['changed'] is True and result['after']['segments'] == 1,
            'Expected a real many-segment -> one-segment merge')
    return result


def overlap_round(ctl, writer, merger, gate_ns, key, start, marker, commit, summary):
    """Publish two uncommitted segments, gate active INSERT, then merge while it waits."""
    before = stats(ctl)
    writer.run(f"BEGIN; INSERT INTO {TABLE} SELECT n,'sharedterm {marker}' "
               f'FROM generate_series({start},{start+1}) n;')
    published = stats(ctl)
    require(published['segments'] > before['segments'], 'Writer did not publish uncommitted segments')
    ctl.run(f'SELECT pg_advisory_lock({gate_ns},{key}),pg_advisory_lock({gate_ns},{key+1});')
    writer.send(f'WITH gate AS MATERIALIZED (SELECT pg_advisory_xact_lock({gate_ns},{key})) '
                f"INSERT INTO {TABLE} SELECT n,'sharedterm {marker}' "
                f'FROM gate CROSS JOIN generate_series({start+2},{start+4}) n;')
    merger.send(f'WITH gate AS MATERIALIZED (SELECT pg_advisory_xact_lock({gate_ns},{key+1})) '
                f'SELECT plumb.merge_index({literal(INDEX_REL)}::regclass) FROM gate;')
    overlap = wait_for(ctl, [writer, merger],
                       lambda rows: all(r['state'] == 'active' and r['wait_event'] == 'advisory' for r in rows),
                       'active INSERT and merge SELECT at independent gates')
    require(next(r for r in overlap if r['pid'] == writer.pid)['xid'] is not None,
            'Writer must have assigned xid before merge')
    ctl.run(f'SELECT pg_advisory_unlock({gate_ns},{key+1});')
    merged = json.loads(merger.finish())
    require(merged['changed'] is True and merged['after']['segments'] == 1,
            'Merge with open writer must consolidate')
    require(merged['before']['segments'] == published['segments']
            and merged['before']['payload_bytes'] == published['payload_bytes'],
            'Merge input must include the writer-published uncommitted segments')
    still_open = wait_for(ctl, [writer],
                          lambda rows: rows[0]['state'] == 'active' and rows[0]['wait_event'] == 'advisory',
                          'writer remains in active INSERT after merge completes')
    compare(ctl, marker + ' invisible while open', marker, [], summary)
    ctl.run(f'SELECT pg_advisory_unlock({gate_ns},{key});')
    writer.finish()
    # This second INSERT appends onto the root published while the transaction was open.
    writer.run('COMMIT;' if commit else 'ROLLBACK;')
    expected = list(range(start, start+5)) if commit else []
    compare(ctl, marker + ' after transaction', marker, expected, summary)
    summary['append_merge_overlap'].append({'commit': commit, 'before': before,
        'published_uncommitted': published, 'simultaneous_statements': overlap,
        'merge_result': merged, 'writer_still_waiting': still_open, 'after': stats(ctl)})


def dual_mergers(ctl, first, second, gate_ns, summary):
    before = stats(ctl)
    require(before['segments'] > 1, 'Need multiple segments before concurrent mergers')
    ctl.run(f'SELECT pg_advisory_lock({gate_ns},50),pg_advisory_lock({gate_ns},51);')
    for session, key in [(first, 50), (second, 51)]:
        session.send(f'WITH gate AS MATERIALIZED (SELECT pg_advisory_xact_lock({gate_ns},{key})) '
                     f'SELECT plumb.merge_index({literal(INDEX_REL)}::regclass) FROM gate;')
    evidence = wait_for(ctl, [first, second],
                        lambda rows: all(r['state'] == 'active' and r['wait_event'] == 'advisory' for r in rows),
                        'two active merge SELECT statements')
    ctl.run(f'SELECT pg_advisory_unlock({gate_ns},50),pg_advisory_unlock({gate_ns},51);')
    results = [json.loads(first.finish()), json.loads(second.finish())]
    require(sorted(r['changed'] for r in results) == [False, True],
            'Concurrent mergers must serialize into one change and one no-op')
    require(stats(ctl)['segments'] == 1, 'Dual merger final root must have one segment')
    summary['dual_mergers'] = {'before': before, 'overlap': evidence, 'results': results, 'after': stats(ctl)}


def ddl_lock_order(ctl, a, b, summary):
    """Separate heap: never conflict with the old RR fixture's AccessShare lock."""
    ctl.run(f'CREATE TABLE {DDL_TABLE}(id integer PRIMARY KEY,body text) WITH(autovacuum_enabled=false); '
            f'CREATE INDEX ddl_default ON {DDL_TABLE} USING plumb(body); '
            f"INSERT INTO {DDL_TABLE} SELECT n,'ddlold' FROM generate_series(1,6) n;")
    oid = ctl.scalar(f'SELECT to_json({literal(DDL_INDEX)}::regclass::oid);')
    heap_oid = ctl.scalar(f'SELECT to_json({literal(DDL_TABLE)}::regclass::oid);')
    a.run(f'BEGIN; LOCK TABLE {DDL_TABLE} IN ACCESS EXCLUSIVE MODE;')
    # Numeric OID avoids any incidental name resolution on the index while waiting.
    b.send(f'SELECT plumb.merge_index({oid}::oid::regclass);')
    activity = wait_for(ctl, [b],
                        lambda rows: rows[0]['state'] == 'active' and rows[0]['wait_event_type'] == 'Lock',
                        'merger blocked on DDL heap')
    locks = ctl.scalar("SELECT coalesce(json_agg(json_build_object('relation',relation,'mode',mode,"
                       "'granted',granted)), '[]'::json) FROM pg_locks "
                       f"WHERE pid={b.pid} AND locktype='relation';")
    require(any(r['relation'] == heap_oid and r['mode'] == 'AccessShareLock' and not r['granted']
                for r in locks), 'Merger must wait for heap AccessShareLock')
    require(not any(r['relation'] == oid for r in locks),
            'LOCK ORDER REGRESSION: waiting merger already holds/requests index lock')
    started = time.monotonic()
    a.run(f'TRUNCATE {DDL_TABLE}; COMMIT;')
    result = json.loads(b.finish())
    require(result['changed'] is False, 'Merge following TRUNCATE must see fresh empty index')
    require(stats(ctl, DDL_INDEX)['segments'] == 0, 'TRUNCATE root not empty')
    ctl.run(f"INSERT INTO {DDL_TABLE} VALUES(9001,'ddlafter'),(9002,'ddlafter');")
    compare(ctl, 'DDL continued inserts', 'ddlafter', [9001, 9002], summary, DDL_TABLE, 'ddl_default')
    compare(ctl, 'DDL old absent', 'ddlold', [], summary, DDL_TABLE, 'ddl_default')
    summary['ddl_lock_order'] = {'blocked_activity': activity, 'blocked_relation_locks': locks,
        'truncate_and_merge_seconds': time.monotonic()-started, 'merge_result': result,
        'after': stats(ctl, DDL_INDEX)}


def verify(ctl, summary):
    saved = ctl.scalar(f'SELECT detail FROM {SCHEMA}.manifest WHERE label=\'complete\';')
    require(saved['expected'] == FINAL, 'Recovery manifest expected IDs mismatch')
    before = {'docs': stats(ctl), 'ddl': stats(ctl, DDL_INDEX)}
    require(before == saved['stats'], 'Recovery stats differ from committed manifest')
    for query, expected in FINAL.items():
        compare(ctl, 'recovery ' + query, query, expected, summary)
    count = ctl.scalar(f'SELECT to_json(count(*)) FROM {TABLE};')
    require(count == 55, 'Final physical visible row count must be 55')
    compare(ctl, 'recovery DDL', 'ddlafter', [9001, 9002], summary, DDL_TABLE, 'ddl_default')
    compare(ctl, 'recovery DDL old', 'ddlold', [], summary, DDL_TABLE, 'ddl_default')
    require(ctl.scalar(f'SELECT to_json(count(*)) FROM {DDL_TABLE};') == 2, 'DDL fixture row count')
    after = {'docs': stats(ctl), 'ddl': stats(ctl, DDL_INDEX)}
    require(before == after, 'Read-only verification changed stats')
    summary['recovery'] = {'stats_before': before, 'stats_after': after, 'docs_count': count,
                           'ddl_count': 2, 'counts': {q: len(ids) for q, ids in FINAL.items()}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True, help='absolute local Unix socket directory')
    parser.add_argument('--port', required=True, type=int)
    parser.add_argument('--database', required=True)
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--log', default='/agent/workspace/checkpoint004-merge-concurrency.log')
    parser.add_argument('--summary', default='/agent/workspace/checkpoint004-merge-concurrency-summary.json')
    args = parser.parse_args()
    require(args.database == 'plumb_postings_checkpoint004', 'Refusing non-checkpoint004 database')
    require(Path(args.host).is_absolute() and Path(args.host).is_dir(), 'Local absolute socket directory required')
    require(1 <= args.port <= 65535, 'Invalid port')
    require(stat.S_ISSOCK((Path(args.host) / ('.s.PGSQL.' + str(args.port))).stat().st_mode),
            'Expected local PostgreSQL Unix socket')
    repo = Path(__file__).resolve().parents[1]
    for output in (args.log, args.summary):
        require(not Path(output).resolve().is_relative_to(repo), 'Logs/summary must be outside repository')
        require(Path(output).resolve().parent.is_dir(), 'Output parent directory missing')
    require(Path(args.log).resolve() != Path(args.summary).resolve(), 'Log and summary paths must differ')
    sessions = []
    summary = {'status': 'running', 'host': args.host, 'port': args.port, 'database': args.database,
               'schema': SCHEMA, 'verify_only': args.verify_only, 'queries': [], 'append_merge_overlap': [],
               'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
               'authorization': 'No global roles created; non-superuser owner/outsider 42501 gate deferred to pg_tests.'}
    mutex = threading.Lock()
    ctl = None
    with open(args.log, 'a', encoding='utf-8') as logfile:
        def log(message):
            with mutex:
                logfile.write(datetime.datetime.now(datetime.timezone.utc).isoformat() + ' ' + message + '\n')
                logfile.flush()
        try:
            ctl = Session('merge_controller', args, log, sessions)
            safe = ctl.scalar("SELECT json_build_object('marker',(SELECT count(*)=1 FROM "
                              "plumb_postings_demo.identity WHERE marker='postings-demo-v1' "
                              "AND database_name=current_database() AND database_oid="
                              "(SELECT oid FROM pg_database WHERE datname=current_database())),"
                              "'owner',(SELECT pg_get_userbyid(datdba)=current_user FROM pg_database "
                              "WHERE datname=current_database()),'version',current_setting('server_version'),"
                              "'fsync',current_setting('fsync'),'full_page_writes',current_setting('full_page_writes'));")
            require(safe['marker'] and safe['owner'], 'Demo identity/database ownership mismatch')
            require(safe['fsync'] == safe['full_page_writes'] == 'on', 'Durability settings must be on')
            summary['identity'] = safe
            if args.verify_only:
                ctl.run('BEGIN READ ONLY;')
                verify(ctl, summary)
                ctl.run('COMMIT;')
            else:
                ctl.run(f"BEGIN; DO $$ BEGIN IF EXISTS(SELECT FROM pg_namespace WHERE nspname='{SCHEMA}') "
                        "THEN RAISE EXCEPTION 'Refusing existing merge schema; never drop/overwrite'; END IF; END $$; "
                        f'CREATE SCHEMA {SCHEMA}; CREATE TABLE {TABLE}(id integer PRIMARY KEY,body text NOT NULL) '
                        'WITH(autovacuum_enabled=false); '
                        f'CREATE INDEX {INDEX} ON {TABLE} USING plumb(body); '
                        f'CREATE TABLE {SCHEMA}.manifest(label text PRIMARY KEY,detail jsonb NOT NULL); '
                        f"INSERT INTO {TABLE} SELECT n,'oldterm sharedterm' FROM generate_series(1,40) n; COMMIT;")
                initial = stats(ctl)
                require(initial['storage'] == 'postings_v1' and initial['format_version'] == 1
                        and initial['segments'] > 1, 'Default post-index INSERTs must produce many v1 segments')
                summary['initial_stats'] = initial
                rr = Session('merge_old_rr', args, log, sessions)
                rr.run('BEGIN ISOLATION LEVEL REPEATABLE READ;')
                summary['old_snapshot'] = rr.scalar('SELECT to_json(pg_current_snapshot()::text);')
                compare(rr, 'anchored oldterm', 'oldterm', list(range(1, 41)), summary)
                writer = Session('merge_writer', args, log, sessions)
                merger = Session('merge_worker', args, log, sessions)
                second = Session('merge_worker_two', args, log, sessions)
                writer.run(f"BEGIN; UPDATE {TABLE} SET body='newterm sharedterm' WHERE id BETWEEN 1 AND 10; "
                           f'DELETE FROM {TABLE} WHERE id BETWEEN 11 AND 15; '
                           f"INSERT INTO {TABLE} SELECT n,'newterm sharedterm' FROM generate_series(101,110) n; COMMIT;")
                summary['mutation_merge'] = merge(merger)
                compare(rr, 'old RR after update/delete/merge', 'oldterm', list(range(1, 41)), summary)
                compare(rr, 'old RR excludes newterm', 'newterm', [], summary)
                compare(ctl, 'new snapshot oldterm', 'oldterm', FINAL['oldterm'], summary)
                compare(ctl, 'new snapshot newterm', 'newterm', FINAL['newterm'], summary)
                gate_ns = int(uuid.uuid4().hex[:7], 16)
                overlap_round(ctl, writer, merger, gate_ns, 10, 201, 'committedmarker', True, summary)
                overlap_round(ctl, writer, merger, gate_ns, 20, 301, 'abortedmarker', False, summary)
                writer.run(f"INSERT INTO {TABLE} SELECT n,'continuedmarker sharedterm' FROM generate_series(401,405) n;")
                dual_mergers(ctl, merger, second, gate_ns, summary)
                compare(rr, 'old RR survives every merge', 'oldterm', list(range(1, 41)), summary)
                compare(rr, 'old RR excludes committed inserts', 'committedmarker', [], summary)
                compare(rr, 'old RR sharedterm exact', 'sharedterm', list(range(1, 41)), summary)
                rr.run('COMMIT;')
                ddl_lock_order(ctl, writer, merger, summary)
                for query, expected in FINAL.items():
                    compare(ctl, 'final ' + query, query, expected, summary)
                manifest = {'expected': FINAL, 'stats': {'docs': stats(ctl), 'ddl': stats(ctl, DDL_INDEX)}}
                ctl.run(f"INSERT INTO {SCHEMA}.manifest VALUES('complete',{literal(json.dumps(manifest))}::jsonb);")
                verify(ctl, summary)
            summary['status'] = 'passed'
        except Exception as exc:
            summary['status'] = 'failed'
            summary['error'] = str(exc)
            summary['traceback'] = traceback.format_exc()
            log(summary['traceback'])
        finally:
            # Cancel only our own known backend PIDs and release our gates; no global cleanup.
            if ctl is not None and ctl.pending is None and ctl.proc.poll() is None:
                try:
                    ctl.run('ROLLBACK; SELECT pg_advisory_unlock_all();')
                    for session in sessions[1:]:
                        if session.pending is not None and session.proc.poll() is None:
                            ctl.run(f'SELECT pg_cancel_backend({session.pid});')
                except Exception as exc:
                    summary['cleanup_warning'] = str(exc)
            for session in reversed(sessions):
                try:
                    session.close()
                except Exception as exc:
                    summary.setdefault('cleanup_errors', []).append(str(exc))
            summary['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            Path(args.summary).write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'status': summary['status'], 'summary': args.summary,
                      'counts': summary.get('recovery', {}).get('counts'), 'error': summary.get('error')}))
    return 0 if summary['status'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
