#!/usr/bin/env python3
# Copyright (C) 2026 Plumb contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Disposable LOCAL PostgreSQL postings concurrency test; standard library + psql.

Run after postings_demo.sql, with explicit --host /absolute/socket/directory
--port 55440 --database plumb_postings_demo. Refuses an existing concurrency
schema. Never drops/rebuilds fixtures, changes other schemas, or manages a server.
Leaves committed fixtures for recovery verification. On success the docs IDs are
1..200 plus 301..310; beacon IDs are 50,100,150,200 plus 301..310;
abortedmarker IDs are 301..310 (rolled-back IDs 201..220 must remain absent).

Both writers perform a first insert, then wait inside active INSERT statements
on distinct advisory gates. The controller observes BOTH blocked statements,
with assigned transaction IDs, before releasing both gates in one SQL statement.
Both transactions remain open until their 100-row writes are verified complete.
A repeatable-read snapshot is anchored before either writer starts.
"""
import argparse
import datetime
import getpass
import json
import os
from pathlib import Path
import queue
import shlex
import socket
import stat
import subprocess
import sys
import threading
import time
import traceback
import uuid

SCHEMA = 'plumb_postings_concurrency'
TABLE = SCHEMA + '.docs'
INDEX = 'docs_postings'
SEQUENTIAL = ('SET enable_seqscan=on; SET enable_bitmapscan=off; '
              'SET enable_indexscan=off; SET enable_indexonlyscan=off;')
BITMAP = ('SET enable_seqscan=off; SET enable_bitmapscan=on; '
          'SET enable_indexscan=off; SET enable_indexonlyscan=off;')


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def literal(value):
    return "'" + value.replace("'", "''") + "'"


class Session:
    def __init__(self, name, args, log, sessions):
        self.name, self.log, self.pending = name, log, None
        self.lines = queue.Queue()
        env = {k: v for k, v in os.environ.items() if not k.startswith('PG')}
        env.update(PGPASSFILE='/dev/null', PGCONNECT_TIMEOUT='5',
                   PGAPPNAME='plumb-concurrency-' + name)
        cmd = ['psql', '-X', '-q', '-A', '-t', '-w', '-v', 'ON_ERROR_STOP=1',
               '--host', args.host, '--port', str(args.port),
               '--dbname', args.database, '--username', getpass.getuser()]
        log(name + ' COMMAND ' + shlex.join(cmd))
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     text=True, bufsize=1, env=env)
        sessions.append(self)
        self.thread = threading.Thread(target=self.pump, daemon=True)
        self.thread.start()
        self.run("SET search_path=pg_catalog,public; SET statement_timeout='60s'; "
                 "SET lock_timeout='45s'; SET idle_in_transaction_session_timeout='120s'; "
                 "SET synchronous_commit=on; SET max_parallel_workers_per_gather=0; "
                 "SET jit=off; SET work_mem='64MB';")
        identity = self.scalar("SELECT json_build_object('database',current_database(),"
                               "'pid',pg_backend_pid(),'local',inet_server_addr() IS NULL);")
        require(identity['database'] == args.database and identity['local'],
                'Connection identity mismatch or non-local connection')
        self.pid = identity['pid']

    def pump(self):
        for line in self.proc.stdout:
            self.log(self.name + ' OUT ' + line.rstrip())
            self.lines.put(line.rstrip('\n'))
        self.lines.put(None)

    def send(self, sql):
        require(self.pending is None, 'Concurrent command on same session')
        self.pending = '__DONE_' + uuid.uuid4().hex
        self.log(self.name + ' SQL ' + sql)
        self.proc.stdin.write(sql + '\n\\echo ' + self.pending + '\n')
        self.proc.stdin.flush()

    def finish(self, timeout=75):
        marker = self.pending
        require(marker is not None, 'No pending command')
        result = []
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, self.name + ' marker timeout')
            try:
                line = self.lines.get(timeout=remaining)
            except queue.Empty:
                raise RuntimeError(self.name + ' marker timeout') from None
            require(line is not None, self.name + ' psql exited: ' + '\n'.join(result))
            if line == marker:
                self.pending = None
                require(not any('ERROR:' in x or 'FATAL:' in x for x in result),
                        self.name + ' SQL error: ' + '\n'.join(result))
                return '\n'.join(result).strip()
            result.append(line)

    def run(self, sql):
        self.send(sql)
        return self.finish()

    def scalar(self, sql):
        return json.loads(self.run(sql))

    def close(self):
        if self.proc.poll() is None:
            try:
                self.proc.stdin.write('ROLLBACK;\n\\q\n')
                self.proc.stdin.flush()
                self.proc.wait(timeout=3)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=3)
        self.thread.join(timeout=2)
        for stream in (self.proc.stdin, self.proc.stdout):
            stream.close()


def nodes(plan):
    root = plan[0]['Plan']
    pending = [root]
    while pending:
        node = pending.pop()
        yield node
        pending.extend(node.get('Plans', []))


def compare(session, label, query, expected, summary):
    statement = f'SELECT id FROM {TABLE} WHERE body OPERATOR(pg_catalog.~~>) {literal(query)}'
    record = {'label': label, 'query': query, 'expected_ids': expected}
    for route, settings in [('seq', SEQUENTIAL), ('bitmap', BITMAP)]:
        session.run(settings)
        plan = session.scalar('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' + statement + ';')
        ids = session.scalar("SELECT coalesce(json_agg(id ORDER BY id),'[]'::json) FROM (" + statement + ') q;')
        record[route + '_ids'] = ids
        record[route + '_count'] = len(ids)
        record[route + '_plan'] = plan
        require(ids == expected, label + ': wrong ' + route + ' IDs')
        if route == 'seq':
            require(any(n['Node Type'] == 'Seq Scan' and n.get('Relation Name') == 'docs'
                        for n in nodes(plan)), label + ': no sequential reference')
        else:
            require(any(n['Node Type'] == 'Bitmap Index Scan' and n.get('Index Name') == INDEX
                        for n in nodes(plan)), label + ': wrong bitmap index route')
    summary['queries'].append(record)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True, help='absolute Unix socket directory only')
    parser.add_argument('--port', required=True, type=int)
    parser.add_argument('--database', required=True)
    parser.add_argument('--log', default='/agent/workspace/postings-concurrency.log')
    parser.add_argument('--summary', default='/agent/workspace/postings-concurrency-summary.json')
    args = parser.parse_args()
    require(args.database.startswith('plumb_postings_') and len(args.database) > 15,
            'Refusing database outside plumb_postings_ prefix')
    require(Path(args.host).is_absolute() and Path(args.host).is_dir(),
            'Refusing non-absolute/nonexistent Unix socket directory')
    require(1 <= args.port <= 65535, 'Invalid port')
    require(stat.S_ISSOCK((Path(args.host) / ('.s.PGSQL.' + str(args.port))).stat().st_mode),
            'Expected local PostgreSQL Unix socket')
    repo = Path(__file__).resolve().parents[1]
    for output in (args.log, args.summary):
        require(not Path(output).resolve().is_relative_to(repo), 'Logs/summary must be outside repo')
    sessions = []
    summary = {'status': 'running', 'database': args.database, 'host': args.host,
               'port': args.port, 'schema': SCHEMA, 'queries': [],
               'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat()}
    lock = threading.Lock()
    with open(args.log, 'a', encoding='utf-8') as logfile:
        def log(message):
            with lock:
                logfile.write(datetime.datetime.now(datetime.timezone.utc).isoformat() + ' ' + message + '\n')
                logfile.flush()
        try:
            ctl = Session('controller', args, log, sessions)
            safe = ctl.scalar("SELECT json_build_object('marker',(SELECT count(*)=1 FROM "
                              "plumb_postings_demo.identity WHERE marker='postings-demo-v1' "
                              "AND database_name=current_database() AND database_oid="
                              "(SELECT oid FROM pg_database WHERE datname=current_database())),"
                              "'owner',(SELECT pg_get_userbyid(datdba)=current_user FROM pg_database "
                              "WHERE datname=current_database()),'version',current_setting('server_version'),"
                              "'fsync',current_setting('fsync'),'full_page_writes',current_setting('full_page_writes'));")
            require(safe['marker'] and safe['owner'], 'Refusing database: demo identity/ownership mismatch')
            require(safe['fsync'] == safe['full_page_writes'] == 'on', 'Durability settings disabled')
            summary['identity'] = safe
            ctl.run(f"BEGIN; DO $$ BEGIN IF EXISTS (SELECT FROM pg_namespace WHERE nspname='{SCHEMA}') "
                    "THEN RAISE EXCEPTION 'Refusing: concurrency schema already exists; never DROP/overwrite'; "
                    f"END IF; END $$; CREATE SCHEMA {SCHEMA}; "
                    f"CREATE TABLE {TABLE}(id integer PRIMARY KEY,body text) WITH(autovacuum_enabled=false); "
                    f"CREATE INDEX {INDEX} ON {TABLE} USING plumb(body) WITH(storage='postings_v1'); COMMIT;")
            summary['schema_created'] = True
            rr = Session('reader', args, log, sessions)
            rr.run('BEGIN ISOLATION LEVEL REPEATABLE READ;')
            summary['reader_snapshot'] = rr.scalar("SELECT to_json(pg_current_snapshot()::text);")
            compare(rr, 'rr_before', 'concurrent', [], summary)
            a = Session('writer_a', args, log, sessions)
            b = Session('writer_b', args, log, sessions)
            # Pair keys are scoped by database and held only for this test duration.
            gate = (int(uuid.uuid4().hex[:7], 16), int(uuid.uuid4().hex[:7], 16))
            ctl.run(f'SELECT pg_advisory_lock({gate[0]},{gate[1]}),pg_advisory_lock({gate[0]},{gate[1]+1});')
            for writer, start, key in [(a, 1, gate[1]), (b, 101, gate[1]+1)]:
                writer.run(f"BEGIN; INSERT INTO {TABLE} VALUES({start},'concurrent cedar');")
                # A MATERIALIZED one-row gate CTE forces the lock before remaining rows.
                writer.send(f"WITH gate AS MATERIALIZED (SELECT pg_advisory_xact_lock({gate[0]},{key})) "
                            f"INSERT INTO {TABLE} SELECT n,'concurrent cedar' || CASE WHEN n%50=0 THEN ' beacon' ELSE '' END "
                            f"FROM gate CROSS JOIN generate_series({start+1},{start+99}) n;")
            deadline = time.monotonic() + 15
            while True:
                ctl.run('SELECT pg_stat_clear_snapshot();')
                evidence = ctl.scalar("SELECT json_agg(json_build_object('pid',pid,'state',state,'xid',backend_xid::text,"
                                      "'wait_event_type',wait_event_type,'wait_event',wait_event,'query',query,"
                                      "'xact_start',xact_start,'query_start',query_start)) FROM pg_stat_activity "
                                      f"WHERE pid IN ({a.pid},{b.pid});")
                if len(evidence) == 2 and all(e['state'] == 'active' and e['xid'] and e['wait_event'] == 'advisory' for e in evidence):
                    break
                require(time.monotonic() < deadline, 'Both writer INSERT statements did not overlap at gates')
                time.sleep(0.05)
            summary['overlap_evidence'] = evidence
            ctl.run(f'SELECT pg_advisory_unlock({gate[0]},{gate[1]}),pg_advisory_unlock({gate[0]},{gate[1]+1});')
            a.finish()
            b.finish()
            for writer in (a, b):
                require(writer.scalar(f'SELECT count(*) FROM {TABLE};') == 100, 'Writer must see only own 100 uncommitted rows')
            compare(rr, 'rr_both_writers_uncommitted', 'concurrent', [], summary)
            compare(ctl, 'fresh_both_writers_uncommitted', 'concurrent', [], summary)
            a.send('COMMIT;')
            b.send('COMMIT;')
            a.finish()
            b.finish()
            compare(rr, 'rr_after_both_commits', 'concurrent', [], summary)
            compare(ctl, 'new_snapshot_after_both_commits', 'concurrent', list(range(1, 201)), summary)
            rr.run('COMMIT;')
            compare(rr, 'reader_new_snapshot', 'concurrent', list(range(1, 201)), summary)
            rare = compare(ctl, 'rare_after_concurrent_commits', 'beacon', [50, 100, 150, 200], summary)
            heap = next(n for n in nodes(rare['bitmap_plan']) if n['Node Type'] == 'Bitmap Heap Scan')
            require(heap.get('Exact Heap Blocks', 0) > 0 and heap.get('Lossy Heap Blocks') == 0,
                    'Rare route must have positive exact heap blocks and zero lossy blocks')
            summary['rare_exact_heap_blocks'] = heap['Exact Heap Blocks']
            summary['rare_lossy_heap_blocks'] = heap['Lossy Heap Blocks']
            abort = Session('aborting_writer', args, log, sessions)
            abort.run(f"BEGIN; INSERT INTO {TABLE} SELECT n,'concurrent beacon abortedmarker' FROM generate_series(201,220) n;")
            compare(abort, 'aborting_writer_own_rows', 'abortedmarker', list(range(201, 221)), summary)
            compare(ctl, 'aborting_writer_not_visible', 'abortedmarker', [], summary)
            abort.run('ROLLBACK;')
            rolled_back = compare(ctl, 'after_rollback_candidates_invisible', 'abortedmarker', [], summary)
            idx = next(n for n in nodes(rolled_back['bitmap_plan']) if n['Node Type'] == 'Bitmap Index Scan')
            require(idx['Actual Rows'] >= 20, 'Expected retained index candidates for rolled-back rows')
            summary['aborted_stored_candidates'] = idx['Actual Rows']
            ctl.run(f"INSERT INTO {TABLE} SELECT n,'concurrent beacon abortedmarker recovered' FROM generate_series(301,310) n;")
            expected = list(range(1, 201)) + list(range(301, 311))
            compare(ctl, 'final_all', 'concurrent', expected, summary)
            compare(ctl, 'final_rare', 'beacon', [50, 100, 150, 200] + list(range(301, 311)), summary)
            compare(ctl, 'subsequent_insert_same_aborted_term', 'abortedmarker', list(range(301, 311)), summary)
            final_ids = ctl.scalar(f"SELECT json_agg(id ORDER BY id) FROM {TABLE};")
            require(final_ids == expected, 'Final whole-table IDs differ')
            summary['final_ids'] = final_ids
            summary['final_count'] = len(final_ids)
            summary['index_stats'] = ctl.scalar(f"SELECT plumb.index_stats('{SCHEMA}.{INDEX}'::regclass)::jsonb;")
            summary['permanent_relations'] = ctl.scalar("SELECT json_agg(json_build_object('name',relname,'persistence',relpersistence,"
                                                     "'options',reloptions)) FROM pg_class "
                                                     f"WHERE oid IN ('{TABLE}'::regclass,'{SCHEMA}.{INDEX}'::regclass);")
            require(all(r['persistence'] == 'p' for r in summary['permanent_relations']), 'Nonpermanent fixtures')
            summary['status'] = 'passed'
            log('PASS: concurrency, MVCC, abort candidates, exact bitmap, subsequent writes')
        except Exception as exc:
            summary['status'] = 'failed'
            summary['error'] = str(exc)
            log(traceback.format_exc())
        finally:
            # Closing writer connections rolls back unfinished transactions. No DROP.
            for session in reversed(sessions):
                try:
                    session.close()
                except Exception as exc:
                    log('session cleanup error: ' + repr(exc))
                    summary.setdefault('cleanup_errors', []).append(str(exc))
            summary['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            Path(args.summary).write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'status': summary['status'], 'summary': args.summary,
                      'log': args.log, 'final_count': summary.get('final_count'),
                      'error': summary.get('error')}))
    return 0 if summary['status'] == 'passed' and not summary.get('cleanup_errors') else 1


if __name__ == '__main__':
    sys.exit(main())
