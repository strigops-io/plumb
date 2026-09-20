#!/usr/bin/env python3
# Copyright (C) 2026 Plumb contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Explicit two-service, owned-fixture parity runner. Python stdlib + psql only.

Importing this module performs no I/O. See parity-contract.md for the safety and
measurement contract. Hosted TIN identity is an operator assertion, not inferred.
"""
from __future__ import annotations

import argparse
from collections import Counter
import csv
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import re
import statistics
import struct
import subprocess
import sys
import tempfile
import time

CORPUS_VERSION = "v1"
MAX_ROWS = 200000
MAX_RESULTS = 200008
MAX_CASES = 100
TIN_LABELS = ("unverified", "public-lead", "planetscale-tin")
DEFAULT_CASES = [
    {"name": "common", "query": "common"},
    {"name": "medium", "query": "medium"},
    {"name": "rare", "query": "rare"},
    {"name": "and", "query": "common AND medium"},
    {"name": "or", "query": "rare OR medium"},
    {"name": "absent", "query": "unfindablexyzzy"},
    {"name": "phrase", "query": '"quick brown"'},
    {"name": "not", "query": "common AND NOT medium"},
    {"name": "wildcard", "query": "pref*"},
    {"name": "unicode", "query": "café"},
]
CAVEATS = [
    "Hosted PlanetScale TIN parity is unverified unless this report was produced against an authorized hosted TIN service. An extension name is not provenance.",
    "Public Lead/local TIN runs validate the harness, not real hosted TIN parity; the TIN label is an unverified operator assertion.",
    "Endpoint identity is an accident guard, not proof against aliases or proxies.",
    "Independent servers share no snapshot. Before/after fingerprints detect drift but cannot rule out intervening ABA mutations.",
    "Different hardware, PostgreSQL majors, services, caches and network prevent generic performance superiority claims.",
    "This bounded deterministic corpus is not full application or full-scale workload coverage. ANSI highlighting is not tested.",
]


class HarnessError(Exception):
    """Only safe, fixed classifications (never raw server stderr)."""


def integer_range(value, low, high, name):
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ValueError(f"{name} outside supported range")
    return value


def validate_run_id(value: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[a-z][a-z0-9_]{0,31}", value) is None:
        raise ValueError("run ID must start with a lowercase letter and contain at most 32 lowercase letters, digits or underscores")
    return value


def validate_service(value: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}", value) is None:
        raise ValueError("service must be a simple libpq service name, not a DSN")
    return value


def quote_ident(value: str) -> str:
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ValueError("invalid SQL identifier")
    return '"' + value.replace('"', '""') + '"'


def quote_literal(value: str) -> str:
    if not isinstance(value, str) or "\x00" in value:
        raise ValueError("invalid SQL text literal")
    # Ordinary strings suffice without backslashes; E strings preserve literal
    # backslashes independently of standard_conforming_strings.
    if "\\" not in value:
        return "'" + value.replace("'", "''") + "'"
    return "E'" + value.replace("\\", "\\\\").replace("'", "''") + "'"


def corpus_rows(rows: int, seed: int):
    integer_range(rows, 1, MAX_ROWS, "rows")
    integer_range(seed, 0, 2**31 - 1, "seed")
    def generate():
        for i in range(1, rows + 1):
            n = (i * 1103515245 + seed * 12345) & 0x7fffffff
            terms = ["common", "document", f"bucket{n % 97}"]
            if n % 10 == 0:
                terms.append("medium")
            if n % 997 == 0:
                terms.append("rare")
            if n % 7 == 0:
                terms.extend(["quick", "brown", "fox", "prefix"])
            if n % 13 == 0:
                terms.append("café")
            yield i, " ".join(terms)
        for offset, body in enumerate((None, "", "café naïve 東京 Straße", "quick brown fox", "quick fox brown", "common rare rare medium prefix", "\\N", 'literal "quotes", comma and backslash \\'), 1):
            yield rows + offset, body
    return generate()


def canonical_row(row):
    if not isinstance(row, (tuple, list)) or len(row) != 2:
        raise ValueError("invalid corpus row")
    ident, body = row
    if isinstance(ident, bool) or not isinstance(ident, int) or not -(2**63) <= ident < 2**63:
        raise ValueError("invalid corpus ID")
    if body is not None and (not isinstance(body, str) or "\x00" in body):
        raise ValueError("invalid corpus body")
    return (json.dumps([ident, body], ensure_ascii=False, separators=(",", ":"), allow_nan=False) + "\n").encode("utf-8")


def corpus_fingerprint(rows) -> str:
    digest = hashlib.sha256()
    for row in rows:
        digest.update(canonical_row(row))
    return digest.hexdigest()


def compare_ids(left: list[int], right: list[int], limit: int = 20) -> dict:
    integer_range(limit, 0, MAX_RESULTS, "example limit")
    for ids in (left, right):
        if not isinstance(ids, list) or any(isinstance(x, bool) or not isinstance(x, int) for x in ids):
            raise ValueError("IDs must be integer lists")
    lc, rc = Counter(left), Counter(right)
    missing, extra = lc - rc, rc - lc
    return {"equal": lc == rc, "left_count": len(left), "right_count": len(right),
            "missing_count": sum(missing.values()), "extra_count": sum(extra.values()),
            "missing_ids": list(sorted(missing.elements()))[:limit],
            "extra_ids": list(sorted(extra.elements()))[:limit]}


def compare_scores(left: dict[int, float], right: dict[int, float], atol: float, rtol: float) -> dict:
    for tolerance in (atol, rtol):
        if isinstance(tolerance, bool) or not isinstance(tolerance, (int, float)) or not math.isfinite(tolerance) or tolerance < 0:
            raise ValueError("score tolerances must be finite and nonnegative")
    for scores in (left, right):
        if not isinstance(scores, dict) or any(isinstance(k, bool) or not isinstance(k, int) for k in scores):
            raise ValueError("score IDs must be integers")
        if any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) for v in scores.values()):
            raise ValueError("scores must be finite numbers")
    missing, extra = sorted(left.keys() - right.keys()), sorted(right.keys() - left.keys())
    mismatches, bits, mismatch_count, bit_count = [], [], 0, 0
    for ident in sorted(left.keys() & right.keys()):
        a, b = left[ident], right[ident]
        if abs(a - b) > atol + rtol * abs(b):
            mismatch_count += 1
            if len(mismatches) < 20:
                mismatches.append({"id": ident, "left": a, "right": b})
        try:
            same_bits = struct.pack("!f", a) == struct.pack("!f", b)
        except (OverflowError, struct.error):
            raise ValueError("score is outside float4 range") from None
        if not same_bits:
            bit_count += 1
            if len(bits) < 20:
                bits.append(ident)
    return {"equal": not (missing or extra or mismatch_count), "missing_ids": missing[:20],
            "extra_ids": extra[:20], "missing_count": len(missing), "extra_count": len(extra),
            "mismatch_count": mismatch_count, "mismatches": mismatches,
            "float4_bits_equal": not (missing or extra or bit_count),
            "float4_bit_mismatch_count": bit_count, "float4_bit_mismatch_ids": bits,
            "atol": atol, "rtol": rtol, "tolerance_rule": "abs(left-right) <= atol + rtol*abs(right)"}


def child_env(service):
    env = os.environ.copy()
    for key in ("PGHOST", "PGHOSTADDR", "PGPORT", "PGDATABASE", "PGUSER", "PGOPTIONS", "PGSERVICE", "PGTARGETSESSIONATTRS", "PGLOADBALANCEHOSTS"):
        env.pop(key, None)
    env["PGSERVICE"] = validate_service(service)
    env["PGCLIENTENCODING"] = "UTF8"
    # TLS, PGSERVICEFILE, PGPASSFILE and authentication configuration are retained.
    return env


def classify_error(stderr):
    text = (stderr or "").lower()
    for needles, code in [(("password", "authentication", "pg_hba"), "authentication_failed"),
                          (("ssl", "tls", "certificate"), "tls_failed"),
                          (("could not connect", "connection refused", "could not translate", "service definition", "connection to server"), "connection_failed"),
                          (("permission denied", "must be owner"), "permission_denied"),
                          (("statement timeout", "canceling statement"), "statement_timeout")]:
        if any(n in text for n in needles):
            return code
    return "psql_failed"


class Endpoint:
    def __init__(self, role, service, timeout=120):
        self.role = role
        self.service = service
        self.timeout = timeout
        self.operator = "~~>" if role == "plumb" else "==>"
        self.metadata = None

    def execute(self, sql, readonly=True, output=None):
        # SQL goes on stdin, never on argv. Fixed flags, no --dbname/DSN.
        command = ["psql", "-X", "-w", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=1"]
        transaction = "BEGIN READ ONLY;" if readonly else "BEGIN;"
        script = (transaction + "\nSET LOCAL search_path = pg_catalog;\n"
                  "SET LOCAL standard_conforming_strings = on;\n"
                  f"SET LOCAL statement_timeout = {self.timeout * 1000};\n"
                  "SET LOCAL extra_float_digits = 3;\n" + sql + "\nCOMMIT;\n")
        started = time.perf_counter()
        try:
            result = subprocess.run(command, input=script, text=True, encoding="utf-8",
                                    stdout=output if output is not None else subprocess.PIPE,
                                    stderr=subprocess.PIPE, env=child_env(self.service), timeout=self.timeout + 10)
        except subprocess.TimeoutExpired:
            raise HarnessError("client_timeout") from None
        except FileNotFoundError:
            raise HarnessError("psql_not_found") from None
        except (OSError, UnicodeError):
            raise HarnessError("psql_transport_failed") from None
        elapsed = (time.perf_counter() - started) * 1000
        if result.returncode:
            raise HarnessError(classify_error(result.stderr))
        return result.stdout, elapsed

    def json(self, sql):
        text, elapsed = self.execute(sql)
        try:
            return json.loads(text), elapsed
        except (ValueError, TypeError):
            raise HarnessError("invalid_server_json") from None

    def preflight(self, schema):
        provider = quote_literal(self.role)
        op = quote_literal(self.operator)
        sql = f"""SELECT json_build_object(
          'identity', json_build_array(current_database(), inet_server_addr()::text,
             COALESCE(inet_server_port(), current_setting('port')::int), EXTRACT(epoch FROM pg_postmaster_start_time())),
          'server_version', current_setting('server_version'),
          'server_encoding', current_setting('server_encoding'),
          'client_encoding', current_setting('client_encoding'),
          'lc_ctype', (SELECT datctype FROM pg_database WHERE datname=current_database()),
          'lc_collate', (SELECT datcollate FROM pg_database WHERE datname=current_database()),
          'provider', {provider},
          'extension_version', (SELECT extversion FROM pg_extension WHERE extname={provider}),
          'am', EXISTS(SELECT 1 FROM pg_am a JOIN pg_proc p ON p.oid=a.amhandler
             JOIN pg_namespace n ON n.oid=p.pronamespace WHERE a.amname={provider} AND n.nspname={provider}),
          'operator', EXISTS(SELECT 1 FROM pg_operator o JOIN pg_namespace n ON n.oid=o.oprnamespace
             JOIN pg_proc p ON p.oid=o.oprcode JOIN pg_namespace pn ON pn.oid=p.pronamespace
             WHERE n.nspname='pg_catalog' AND pn.nspname={provider} AND o.oprname={op} AND o.oprleft='text'::regtype
             AND o.oprright='text'::regtype AND o.oprresult='boolean'::regtype),
          'full_score', EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
             WHERE n.nspname={provider} AND p.proname='full_score' AND p.proargtypes='27'::oidvector
             AND p.prorettype='real'::regtype AND has_schema_privilege(n.oid, 'USAGE')
             AND has_function_privilege(p.oid, 'EXECUTE')),
          'highlight', EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
             WHERE n.nspname={provider} AND p.proname='highlight' AND p.proargtypes='25 25 25 25'::oidvector
             AND p.prorettype='text'::regtype AND has_schema_privilege(n.oid, 'USAGE')
             AND has_function_privilege(p.oid, 'EXECUTE')),
          'schema_exists', EXISTS(SELECT 1 FROM pg_namespace WHERE nspname={quote_literal(schema)}),
          'can_create_schema', has_database_privilege(current_database(), 'CREATE'),
          'in_recovery', pg_is_in_recovery());"""
        data, _ = self.json(sql)
        if not isinstance(data, dict) or not data.get("am") or not data.get("operator"):
            raise HarnessError("missing_canonical_provider_capability")
        self.metadata = data
        return data

    def prepare(self, schema, marker, rows, seed):
        table = quote_ident(schema) + '."docs"'
        path = None
        try:
            # NamedTemporaryFile creates a private (0600) file. Close it before
            # psql opens it locally; never ask the server to read this filename.
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", newline="",
                                             prefix="plumb-parity-", suffix=".csv",
                                             delete=False) as stream:
                path = stream.name
                # psql meta-command parsing is not SQL parsing. Restrict even
                # TMPDIR-derived names before quoting: no newlines, quotes,
                # backslashes, variable interpolation or command metacharacters.
                if re.fullmatch(r"/[A-Za-z0-9_./-]+", path) is None:
                    raise HarnessError("unsafe_copy_file_path")
                # Quote all non-NULL fields, including literal \\N and empty text.
                writer = csv.writer(stream, lineterminator="\n", quoting=csv.QUOTE_ALL)
                for ident, body in corpus_rows(rows, seed):
                    if body is None:
                        stream.write(f'{ident},\\N\n')
                    else:
                        writer.writerow((ident, body))
            # File EOF makes psql send protocol CopyDone, not a legacy inline
            # \\. terminator (which PG18 CSV interprets as a data row).
            # One execute keeps schema, load, index and marker in one transaction.
            sql = (f"CREATE SCHEMA {quote_ident(schema)};\n"
                   f"CREATE TABLE {table} (id bigint PRIMARY KEY, body text);\n"
                   f"\\copy {table} (id,body) FROM {quote_literal(path)} WITH (FORMAT csv, NULL E'\\\\N')\n"
                   f"CREATE INDEX docs_search_idx ON {table} USING {quote_ident(self.role)} (body);\n"
                   f"ANALYZE {table};\n"
                   f"CREATE TABLE {quote_ident(schema)}.fixture_marker (singleton boolean PRIMARY KEY CHECK (singleton), metadata jsonb NOT NULL);\n"
                   f"INSERT INTO {quote_ident(schema)}.fixture_marker VALUES (true, {quote_literal(json.dumps(marker))}::jsonb);")
            self.execute(sql, readonly=False)
        except (OSError, UnicodeError):
            raise HarnessError("copy_file_failed") from None
        finally:
            if path is not None:
                try:
                    os.unlink(path)
                except OSError:
                    raise HarnessError("copy_file_cleanup_failed") from None

    def check_structure(self, schema):
        # Permanent owned tables, exact public columns, primary key and the native
        # valid index are part of the fixture, not just a matching relation name.
        data, _ = self.json(f"""SELECT json_build_object(
          'tables', (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname={quote_literal(schema)} AND c.relname IN ('docs','fixture_marker')
            AND c.relkind='r' AND c.relpersistence='p' AND NOT c.relrowsecurity
            AND NOT c.relforcerowsecurity AND c.relowner=n.nspowner),
          'columns', (SELECT json_agg(json_build_array(a.attname, a.atttypid::regtype::text, a.attnotnull) ORDER BY a.attnum)
            FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname={quote_literal(schema)} AND c.relname='docs' AND a.attnum>0 AND NOT a.attisdropped),
          'primary_key', EXISTS(SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={quote_literal(schema)}
            AND c.relname='docs' AND i.indisprimary AND i.indisvalid AND i.indkey::text='1'),
          'search_index', EXISTS(SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_class ix ON ix.oid=i.indexrelid
            JOIN pg_am am ON am.oid=ix.relam WHERE n.nspname={quote_literal(schema)}
            AND c.relname='docs' AND am.amname={quote_literal(self.role)} AND i.indisvalid AND i.indisready
            AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnkeyatts=1 AND i.indkey::text='2'),
          'search_indexes', (SELECT COALESCE(json_agg(json_build_object(
              'name', ix.relname, 'definition', pg_get_indexdef(i.indexrelid),
              'reloptions', ARRAY(SELECT option FROM unnest(ix.reloptions) option ORDER BY option COLLATE "C"),
              'indnkeyatts', i.indnkeyatts,
              'opclasses', (SELECT json_agg(json_build_object('schema', ns.nspname, 'name', op.opcname) ORDER BY k.ordinality)
                FROM unnest(i.indclass::oid[]) WITH ORDINALITY k(opclass, ordinality)
                JOIN pg_opclass op ON op.oid=k.opclass JOIN pg_namespace ns ON ns.oid=op.opcnamespace)
            ) ORDER BY ix.relname COLLATE "C"), '[]'::json)
            FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_class ix ON ix.oid=i.indexrelid
            JOIN pg_am am ON am.oid=ix.relam WHERE n.nspname={quote_literal(schema)}
            AND c.relname='docs' AND am.amname={quote_literal(self.role)} AND i.indisvalid AND i.indisready));""")
        indexes = data.pop("search_indexes", None) if isinstance(data, dict) else None
        if (data != {"tables": 2, "columns": [["id", "bigint", True], ["body", "text", False]],
                     "primary_key": True, "search_index": True}
                or not isinstance(indexes, list) or len(indexes) != 1):
            raise HarnessError("fixture_structure_mismatch")
        # Observe provider-specific opclasses/options; do not assume that TIN's
        # index opclass is the canonical query operator checked by preflight.
        return indexes

    def marker(self, schema):
        data, _ = self.json(f"SELECT COALESCE(json_agg(metadata), '[]'::json) FROM (SELECT metadata FROM {quote_ident(schema)}.fixture_marker LIMIT 2) m;")
        if not isinstance(data, list) or len(data) != 1 or not isinstance(data[0], dict):
            raise HarnessError("invalid_ownership_marker")
        return data[0]

    def fingerprint(self, schema, max_rows):
        # Stream the wire result through a temporary file rather than accumulating
        # the entire corpus in memory. Never retain raw server connection stderr.
        with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as output:
            self.execute(f"SELECT json_build_array(id, body)::text FROM {quote_ident(schema)}.docs ORDER BY id LIMIT {max_rows + 1};", output=output)
            output.seek(0)
            digest, count, previous = hashlib.sha256(), 0, None
            try:
                for line in output:
                    row = json.loads(line)
                    encoded = canonical_row(row)
                    if previous is not None and row[0] <= previous:
                        raise ValueError("duplicate or unordered ID")
                    previous = row[0]
                    digest.update(encoded)
                    count += 1
                    if count > max_rows:
                        raise HarnessError("fixture_row_cap_exceeded")
            except (ValueError, TypeError, UnicodeError):
                raise HarnessError("invalid_fixture_row") from None
        return {"rows": count, "sha256": digest.hexdigest()}

    def select(self, schema, query, fields, limit, order="id"):
        return (f"SELECT {fields} FROM {quote_ident(schema)}.docs "
                f"WHERE body OPERATOR(pg_catalog.{self.operator}) {quote_literal(query)} "
                f"ORDER BY {order} LIMIT {limit}")

    def records(self, select):
        data, elapsed = self.json(f"SELECT COALESCE(json_agg(r), '[]'::json) FROM ({select}) r;")
        if not isinstance(data, list):
            raise HarnessError("invalid_result_rows")
        return data, elapsed


def make_marker(role, run_id, rows, seed):
    return {"provider": role, "run_id": run_id, "corpus_version": CORPUS_VERSION,
            "seed": seed, "regular_rows": rows, "expected_rows": rows + 8,
            "sha256": corpus_fingerprint(corpus_rows(rows, seed))}


def validate_marker(marker, role, run_id):
    try:
        rows, seed = marker["regular_rows"], marker["seed"]
        expected = make_marker(role, run_id, rows, seed)
    except (KeyError, ValueError, TypeError):
        raise HarnessError("invalid_ownership_marker") from None
    if marker != expected:
        raise HarnessError("ownership_marker_mismatch")
    return expected


def verify_fixture(endpoint, schema, marker):
    indexes = endpoint.check_structure(schema)
    if endpoint.marker(schema) != marker:
        raise HarnessError("ownership_marker_changed")
    actual = endpoint.fingerprint(schema, marker["expected_rows"])
    if actual != {"rows": marker["expected_rows"], "sha256": marker["sha256"]}:
        raise HarnessError("corpus_fingerprint_mismatch")
    # Keep the ownership marker/corpus hash contract unchanged. Index evidence
    # belongs to this observation, not to a newly required marker version.
    return {**actual, "search_indexes": indexes}


def plan_nodes(plan):
    found = []
    def walk(node):
        if isinstance(node, dict):
            if "Node Type" in node:
                found.append({k: node[k] for k in ("Node Type", "Index Name", "Relation Name", "Schema") if k in node})
            for value in node.values():
                walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)
    walk(plan)
    return found


def timing_summary(values):
    if not values or any(not isinstance(v, (int, float)) or not math.isfinite(v) or v < 0 for v in values):
        raise HarnessError("invalid_explain_timing")
    return {"median_ms": statistics.median(values), "min_ms": min(values), "max_ms": max(values)}


def measure(endpoint, select, samples):
    plans, planning, execution, client = [], [], [], []
    for _ in range(samples):
        data, elapsed = endpoint.json("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) " + select + ";")
        try:
            entry = data[0]
            planning.append(entry["Planning Time"])
            execution.append(entry["Execution Time"])
        except (KeyError, TypeError, IndexError):
            raise HarnessError("invalid_explain_result") from None
        client.append(elapsed)
        plans.append({"nodes": plan_nodes(entry), "explain": entry})
    return {"samples": plans, "server_planning": timing_summary(planning),
            "server_execution": timing_summary(execution), "client_round_trip": timing_summary(client),
            "client_scope": "psql startup, connection, transaction and EXPLAIN round trip; not server execution time"}


def load_cases(path):
    if path is None:
        return [dict(x) for x in DEFAULT_CASES]
    try:
        with open(path, "r", encoding="utf-8") as handle:
            text = handle.read(65537)
        if len(text) > 65536:
            raise ValueError("case file too large")
        data = json.loads(text)
        if not isinstance(data, list) or not 1 <= len(data) <= MAX_CASES:
            raise ValueError("invalid cases")
        names = set()
        for case in data:
            if not isinstance(case, dict) or set(case) != {"name", "query"}:
                raise ValueError("invalid case keys")
            if not isinstance(case["name"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", case["name"]):
                raise ValueError("invalid case name")
            if case["name"] in names:
                raise ValueError("duplicate case name")
            names.add(case["name"])
            if not isinstance(case["query"], str) or not 1 <= len(case["query"]) <= 2048 or any(c in case["query"] for c in ("\x00", "\n", "\r")):
                raise ValueError("invalid query text")
        return data
    except (OSError, ValueError, UnicodeError):
        raise HarnessError("invalid_query_case_file") from None


def safe_error(error):
    return str(error) if isinstance(error, HarnessError) else "invalid_result_or_internal_error"


def comparison(endpoints, args, report):
    schema = "plumb_parity_" + args.run_id
    markers, before = {}, {}
    report["markers"], report["fingerprints_before"] = markers, before
    for ep in endpoints:
        if not ep.metadata["schema_exists"]:
            raise HarnessError("fixture_schema_missing")
        markers[ep.role] = validate_marker(ep.marker(schema), ep.role, args.run_id)
        before[ep.role] = verify_fixture(ep, schema, markers[ep.role])
    a, b = (dict(markers[ep.role]) for ep in endpoints)
    a.pop("provider"); b.pop("provider")
    if a != b:
        raise HarnessError("endpoint_fixture_metadata_differ")
    results = report["cases"] = []
    score_nonempty, highlight_nonempty = set(), set()
    incomplete, mismatch = False, False
    for case in load_cases(args.cases_json):
        result = {"name": case["name"], "query": case["query"], "status": "passed", "endpoints": {}}
        results.append(result)
        ids, score_rows, highlight_rows, rankings = {}, {}, {}, {}
        case_incomplete, case_mismatch = False, False
        for ep in endpoints:
            evidence = result["endpoints"][ep.role] = {}
            select = ep.select(schema, case["query"], "id", args.max_results + 1)
            try:
                records, elapsed = ep.records(select)
                row_ids = [r["id"] for r in records]
                if any(isinstance(i, bool) or not isinstance(i, int) for i in row_ids) or len(set(row_ids)) != len(row_ids):
                    raise HarnessError("invalid_logical_ids")
                evidence["match_client_ms"] = elapsed
                evidence["returned_rows"] = len(row_ids)
                if len(row_ids) > args.max_results:
                    raise HarnessError("result_cap_exceeded")
                ids[ep.role] = row_ids
                # Natural planner settings, deliberately no enable_seqscan override.
                evidence["timing"] = measure(ep, select, args.samples)
            except Exception as error:
                evidence["match_error"] = safe_error(error)
                case_incomplete = True
            if args.with_scores:
                try:
                    if not ep.metadata["full_score"]:
                        raise HarnessError("missing_full_score_capability")
                    if ep.role not in ids:
                        raise HarnessError("match_suite_incomplete")
                    fn = quote_ident(ep.role) + ".full_score(ctid)"
                    rows, _ = ep.records(ep.select(schema, case["query"], f"id, {fn} AS score, encode(float4send({fn}), 'hex') AS score_bits", args.max_results + 1))
                    if len(rows) > args.max_results:
                        raise HarnessError("score_result_cap_exceeded")
                    score_ids = [r["id"] for r in rows]
                    # Validate before dict construction: bool and float keys can
                    # alias integer IDs in Python, concealing malformed results.
                    if any(type(ident) is not int for ident in score_ids):
                        raise HarnessError("invalid_score_ids")
                    if len(set(score_ids)) != len(rows) or not compare_ids(ids[ep.role], score_ids)["equal"]:
                        raise HarnessError("score_id_set_changed")
                    if any(type(r["score"]) not in (int, float) or not math.isfinite(r["score"]) for r in rows):
                        raise HarnessError("invalid_score_value")
                    if any(type(r["score_bits"]) is not str or re.fullmatch(r"[0-9a-fA-F]{8}", r["score_bits"]) is None for r in rows):
                        raise HarnessError("invalid_score_bits")
                    mapping = {r["id"]: r["score"] for r in rows}
                    compare_scores(mapping, mapping, args.atol, args.rtol)
                    score_rows[ep.role] = (mapping, {r["id"]: r["score_bits"] for r in rows})
                    if rows:
                        score_nonempty.add(ep.role)
                    rank, _ = ep.records(ep.select(schema, case["query"], f"id, {fn} AS score", args.top_k, "score DESC, id ASC"))
                    rank_ids = [r["id"] for r in rank]
                    if any(type(ident) is not int for ident in rank_ids) or len(set(rank_ids)) != len(rank) or len(rank) != min(args.top_k, len(mapping)) or any(i not in mapping for i in rank_ids):
                        raise HarnessError("invalid_rank_result")
                    rankings[ep.role] = rank_ids
                except Exception as error:
                    evidence["score_error"] = safe_error(error)
                    case_incomplete = True
            if args.with_highlights:
                try:
                    if not ep.metadata["highlight"]:
                        raise HarnessError("missing_highlight_capability")
                    if ep.role not in ids:
                        raise HarnessError("match_suite_incomplete")
                    fn = quote_ident(ep.role) + ".highlight(body, '<b>', '</b>', " + quote_literal(case["query"]) + ")"
                    rows, _ = ep.records(ep.select(schema, case["query"], f"id, {fn} AS highlight", args.max_results + 1))
                    if len(rows) > args.max_results:
                        raise HarnessError("highlight_result_cap_exceeded")
                    mapping = {r["id"]: r["highlight"] for r in rows}
                    if len(mapping) != len(rows) or not compare_ids(ids[ep.role], [r["id"] for r in rows])["equal"]:
                        raise HarnessError("highlight_id_set_changed")
                    if any(not isinstance(value, str) for value in mapping.values()):
                        raise HarnessError("invalid_highlight_text")
                    highlight_rows[ep.role] = mapping
                    if rows:
                        highlight_nonempty.add(ep.role)
                except Exception as error:
                    evidence["highlight_error"] = safe_error(error)
                    case_incomplete = True
        if len(ids) == 2:
            result["matches"] = compare_ids(ids["plumb"], ids["tin"])
            case_mismatch |= not result["matches"]["equal"]
        if args.with_scores and len(score_rows) == 2:
            result["scores"] = compare_scores(score_rows["plumb"][0], score_rows["tin"][0], args.atol, args.rtol)
            wire_left, wire_right = score_rows["plumb"][1], score_rows["tin"][1]
            result["scores"]["float4_wire_bits_equal"] = wire_left == wire_right
            case_mismatch |= not result["scores"]["equal"]
        if args.with_scores and len(rankings) == 2:
            result["rank"] = {"equal": rankings["plumb"] == rankings["tin"], "top_k": args.top_k, "ids": rankings}
            case_mismatch |= not result["rank"]["equal"]
        if args.with_highlights and len(highlight_rows) == 2:
            left, right = highlight_rows["plumb"], highlight_rows["tin"]
            diff = [i for i in sorted(left.keys() | right.keys()) if i not in left or i not in right or left[i] != right[i]]
            result["highlights"] = {"equal": not diff, "mismatch_count": len(diff), "mismatch_ids": diff[:20]}
            case_mismatch |= bool(diff)
        result["status"] = "incomplete" if case_incomplete else "mismatch" if case_mismatch else "passed"
        incomplete |= case_incomplete
        mismatch |= case_mismatch
    suite_errors = report["selected_suite_errors"] = []
    for selected, observed, name in ((args.with_scores, score_nonempty, "scores"),
                                     (args.with_highlights, highlight_nonempty, "highlights")):
        if selected and observed != {"plumb", "tin"}:
            suite_errors.append({"suite": name, "error": "no_successful_nonempty_execution_on_both_endpoints"})
            incomplete = True
    after = report["fingerprints_after"] = {}
    for ep in endpoints:
        try:
            after[ep.role] = verify_fixture(ep, schema, markers[ep.role])
            # DDL/options may legitimately differ between providers. Only compare
            # each endpoint with its own pre-run evidence, retaining both values.
            if after[ep.role]["search_indexes"] != before[ep.role]["search_indexes"]:
                after[ep.role]["error"] = "fixture_index_metadata_changed"
                incomplete = True
        except Exception as error:
            after[ep.role] = {"error": safe_error(error)}
            incomplete = True
    report["status"] = "incomplete" if incomplete else "mismatch" if mismatch else "passed"


def run(args, report):
    endpoints = [Endpoint("plumb", args.plumb_service, args.timeout), Endpoint("tin", args.tin_service, args.timeout)]
    schema = "plumb_parity_" + args.run_id
    report["endpoints"] = {}
    failures = []
    for ep in endpoints:
        try:
            metadata = ep.preflight(schema)
            # Identity is only used for the accident guard, not disclosed in report.
            report["endpoints"][ep.role] = {k: v for k, v in metadata.items() if k != "identity"}
        except Exception as error:
            failures.append(ep.role)
            report["endpoints"][ep.role] = {"error": safe_error(error)}
    if failures:
        raise HarnessError("endpoint_preflight_failed")
    if not all(isinstance(ep.metadata.get("identity"), list) and len(ep.metadata["identity"]) == 4 for ep in endpoints):
        raise HarnessError("invalid_endpoint_identity")
    if endpoints[0].metadata["identity"] == endpoints[1].metadata["identity"]:
        raise HarnessError("identical_endpoint_identity")
    if args.action == "compare":
        comparison(endpoints, args, report)
        return
    if not args.allow_fixture_writes:
        raise HarnessError("fixture_writes_not_authorized")
    if any(ep.metadata["schema_exists"] for ep in endpoints):
        raise HarnessError("existing_schema_refused")
    if any(not ep.metadata["can_create_schema"] or ep.metadata["in_recovery"] for ep in endpoints):
        raise HarnessError("fixture_write_privilege_unavailable")
    preparation = report["preparation"] = {}
    committed = 0
    for ep in endpoints:
        marker = make_marker(ep.role, args.run_id, args.rows, args.seed)
        preparation[ep.role] = {"status": "not_started", "marker": marker}
        try:
            ep.prepare(schema, marker, args.rows, args.seed)
            committed += 1
            preparation[ep.role]["status"] = "committed"
        except (Exception, KeyboardInterrupt) as error:
            # A connection failure during COMMIT may have an indeterminate outcome.
            error_code = "interrupted_commit_outcome_may_be_unknown" if isinstance(error, KeyboardInterrupt) else safe_error(error)
            preparation[ep.role].update(status="failed_or_commit_unknown", error=error_code)
            report["preparation_status"] = "partial_preparation" if committed else "failed_or_commit_unknown"
            report["status"] = "incomplete"
            report["preservation_note"] = "No cleanup attempted; inspect owned schemas before choosing a fresh run ID. Failed connection may leave commit outcome unknown."
            for remaining in endpoints:
                preparation.setdefault(remaining.role, {"status": "not_started"})
            return
    report["preparation_status"] = "prepared"
    report["fingerprints_after"] = {}
    for ep in endpoints:
        report["fingerprints_after"][ep.role] = verify_fixture(ep, schema, preparation[ep.role]["marker"])
    report["status"] = "passed"


class CustomArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        # argparse's message may include unknown flags, values, or credentials.
        # Do not render it or usage (whose program name is also caller supplied).
        self.exit(2, "Invalid arguments; see --help. No connections attempted.\n")


def parser():
    p = CustomArgumentParser(description=__doc__)
    p.add_argument("action", nargs="?", choices=("prepare", "compare"), default="compare")
    p.add_argument("--plumb-service", required=True)
    p.add_argument("--tin-service", required=True)
    p.add_argument("--run-id", required=True)
    p.add_argument("--out-dir", required=True)
    p.add_argument("--allow-fixture-writes", action="store_true")
    p.add_argument("--rows", type=int, default=1000)
    p.add_argument("--seed", type=int, default=1729)
    p.add_argument("--max-results", type=int, default=20000)
    p.add_argument("--samples", type=int, default=3)
    p.add_argument("--top-k", type=int, default=20)
    p.add_argument("--timeout", type=int, default=120)
    p.add_argument("--atol", type=float, default=1e-6)
    p.add_argument("--rtol", type=float, default=1e-5)
    p.add_argument("--with-scores", action="store_true")
    p.add_argument("--with-highlights", action="store_true")
    p.add_argument("--cases-json", "--query-cases", dest="cases_json")
    p.add_argument("--tin-label", choices=TIN_LABELS, default="unverified",
                   help="fixed provenance category; operator assertion only, not verified endpoint identity (default: unverified)")
    return p


def markdown(report):
    lines = ["# Two-instance parity report", "", f"**Status: {report['status']}**", "",
             f"Action: `{report['action']}` · Run: `{report['run_id']}`", ""]
    if "error" in report:
        lines += ["Error classification: `" + report["error"] + "`", ""]
    if "preparation_status" in report:
        lines += ["Preparation: `" + report["preparation_status"] + "`", ""]
    for error in report.get("selected_suite_errors", []):
        lines += [f"Selected suite `{error['suite']}` incomplete: `{error['error']}`.", ""]
    for role, value in report.get("fingerprints_after", {}).items():
        if "sha256" in value:
            lines += [f"- {role}: {value['rows']} fixture rows; SHA256 `{value['sha256']}`."]
    if "cases" in report:
        lines += ["", "| Case | Status | Missing / extra IDs |", "|---|---|---|"]
        for case in report["cases"]:
            counts = case.get("matches", {})
            lines.append(f"| {case['name']} | {case['status']} | {counts.get('missing_count', 'n/a')} / {counts.get('extra_count', 'n/a')} |")
        lines += ["", "## Natural-plan timings", "", "All values are milliseconds; these are per-case sample medians, not throughput claims.", "",
                  "| Case | Role | Server planning | Server execution | Client round trip |", "|---|---|---:|---:|---:|"]
        for case in report["cases"]:
            for role, evidence in case["endpoints"].items():
                timing = evidence.get("timing")
                if timing:
                    lines.append(f"| {case['name']} | {role} | {timing['server_planning']['median_ms']:.3f} | {timing['server_execution']['median_ms']:.3f} | {timing['client_round_trip']['median_ms']:.3f} |")
        lines += [""]
    lines += ["JSON includes ownership/fingerprint evidence, result differences and natural EXPLAIN samples with separate server and client timings.", "", "## Scope and caveats", ""]
    lines += ["- " + caveat for caveat in CAVEATS]
    return "\n".join(lines) + "\n"


def main(argv=None):
    args = parser().parse_args(argv)
    # Validate without echoing unsafe user values. No connection before validation.
    try:
        validate_run_id(args.run_id)
        validate_service(args.plumb_service); validate_service(args.tin_service)
        if args.plumb_service == args.tin_service:
            raise ValueError("services must differ")
        integer_range(args.rows, 1, MAX_ROWS, "rows")
        integer_range(args.seed, 0, 2**31 - 1, "seed")
        integer_range(args.max_results, 1, MAX_RESULTS, "max results")
        integer_range(args.samples, 1, 30, "samples")
        integer_range(args.top_k, 1, 10000, "top k")
        integer_range(args.timeout, 1, 3600, "timeout")
        compare_scores({}, {}, args.atol, args.rtol)
        if type(args.tin_label) is not str or args.tin_label not in TIN_LABELS:
            raise ValueError("invalid label")
    except ValueError:
        print("Invalid argument or unsupported range; see --help. No connections attempted.", file=sys.stderr)
        return 2
    out = Path(args.out_dir)
    try:
        out.mkdir(mode=0o700, parents=False, exist_ok=False)
    except OSError:
        print("Output must be a new directory under an existing parent. No connections attempted.", file=sys.stderr)
        return 2
    report = {"report_version": 1, "created_utc": datetime.now(timezone.utc).isoformat(),
              "action": args.action, "run_id": args.run_id, "status": "incomplete",
              "tin_label": args.tin_label, "caveats": CAVEATS,
              "configuration": {"max_results": args.max_results, "samples": args.samples,
                                "with_scores": args.with_scores, "with_highlights": args.with_highlights,
                                "top_k": args.top_k, "atol": args.atol, "rtol": args.rtol}}
    try:
        if args.action == "prepare" and not args.allow_fixture_writes:
            raise HarnessError("fixture_writes_not_authorized")
        # Validate case input before opening any connection.
        if args.action == "compare":
            load_cases(args.cases_json)
        run(args, report)
    except KeyboardInterrupt:
        report["error"] = "interrupted_commit_outcome_may_be_unknown"
        report["status"] = "incomplete"
    except Exception as error:
        report["error"] = safe_error(error)
        report["status"] = "incomplete"
    try:
        (out / "report.json").write_text(json.dumps(report, indent=2, ensure_ascii=False, allow_nan=False) + "\n", encoding="utf-8")
        (out / "summary.md").write_text(markdown(report), encoding="utf-8")
    except (OSError, ValueError):
        print("Could not write complete report; results are incomplete.", file=sys.stderr)
        return 2
    print("Parity runner status: " + report["status"] + ". Local report.json and summary.md written.")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
