# SPDX-License-Identifier: AGPL-3.0-only
"""Independent, database-free tests for parity-contract.md.

Run: python3 -m unittest discover -s benches -p test_parity.py -v
Pure helper tests retain their independent contract checks. Orchestration tests
exercise main/run with the real Endpoint preflight and prepare SQL builders,
mocking subprocess transport and Endpoint fixture/result reads only.
All subprocess entry points are blocked during import and CLI execution.
"""

import contextlib
import csv
import importlib.util
import io
import json
import math
from pathlib import Path
import re
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


RUNNER = Path(__file__).with_name("parity.py")
PROCESS_APIS = ("run", "Popen", "call", "check_call", "check_output")
SECRET = "DO_NOT_FORWARD_password_SyntheticSecret_7391"


@contextlib.contextmanager
def blocked_processes(effect):
    """Mock transport only, never implement parity logic in the tests."""
    with contextlib.ExitStack() as stack:
        patched = [stack.enter_context(mock.patch.object(subprocess, name,
                   side_effect=effect)) for name in PROCESS_APIS]
        yield patched


def import_runner():
    name = "_independent_parity_under_test"
    spec = importlib.util.spec_from_file_location(name, RUNNER)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module  # dataclasses may inspect their defining module
    try:
        spec.loader.exec_module(module)
    finally:
        sys.modules.pop(name, None)
    return module


def search_index_metadata(role):
    return [{"name": "docs_search_idx",
             "definition": f"CREATE INDEX docs_search_idx ON plumb_parity_test001.docs USING {role} (body)",
             "reloptions": [], "indnkeyatts": 1,
             "opclasses": [{"schema": role, "name": role + "_text_ops"}]}]


def sql_statements(sql):
    """Mask quoted tokens/comments before splitting the fixture SQL on semicolons.

    This is a conservative test oracle, not a general SQL permission checker.
    In particular, scan every token in WITH statements, not just their first word.
    Unterminated quotes/comments fail closed rather than hiding trailing writes.
    """
    if not isinstance(sql, str):
        raise ValueError("expected SQL stdin text")
    masked = list(sql)
    i = 0
    while i < len(sql):
        start = i
        if sql.startswith("--", i):
            end = sql.find("\n", i + 2)
            i = len(sql) if end < 0 else end
        elif sql.startswith("/*", i):
            depth = 1
            i += 2
            while i < len(sql) and depth:
                if sql.startswith("/*", i):
                    depth += 1
                    i += 2
                elif sql.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("unterminated SQL comment")
        elif sql[i] in "'\"":
            quote = sql[i]
            escaped = (quote == "'" and i > 0 and sql[i - 1] in "Ee"
                       and (i < 2 or not (sql[i - 2].isalnum() or sql[i - 2] == "_")))
            i += 1
            while i < len(sql):
                if escaped and sql[i] == "\\":
                    i += 2
                elif sql[i] == quote:
                    i += 1
                    if i < len(sql) and sql[i] == quote:
                        i += 1
                    else:
                        break
                else:
                    i += 1
            else:
                raise ValueError("unterminated SQL quote")
        elif sql[i] == "$" and (tag := re.match(r"\$(?:[A-Za-z_][A-Za-z_0-9]*)?\$", sql[i:])):
            delimiter = tag.group()
            end = sql.find(delimiter, i + len(delimiter))
            if end < 0:
                raise ValueError("unterminated dollar quote")
            i = end + len(delimiter)
        else:
            i += 1
            continue
        masked[start:i] = " " * (i - start)
    return [statement.strip() for statement in "".join(masked).split(";") if statement.strip()]


WRITE_SQL = re.compile(r"\b(?:CREATE|DROP|ALTER|TRUNCATE|INSERT|UPDATE|DELETE|MERGE|COPY|"
                       r"GRANT|REVOKE|CALL|DO|VACUUM|ANALYZE|REINDEX|INTO)\b", re.I)


def assert_readonly_sql(test, sql):
    statements = sql_statements(sql)
    test.assertTrue(statements, "SQL stdin must not be empty")
    test.assertRegex(statements[0], r"(?i)\ABEGIN\s+READ\s+ONLY\Z")
    test.assertEqual(statements[-1].upper(), "COMMIT")
    for statement in statements:
        test.assertNotRegex(statement, WRITE_SQL)
        test.assertRegex(statement, r"(?i)\A(?:BEGIN\s+READ\s+ONLY\s*$|SET\s+LOCAL\b|SELECT\b|WITH\b|COMMIT\s*$)")
        test.assertNotRegex(statement, re.compile(r"\bREAD\s+WRITE\b|\b(?:default_)?transaction_read_only\b", re.I))


class SqlSafetyOracleTests(unittest.TestCase):
    def test_literals_identifiers_and_nested_comments_are_not_commands(self):
        sql = r'''BEGIN READ ONLY;
SELECT has_database_privilege(current_database(), 'CREATE'),
       'DROP; it''s INSERT', E'escaped \' DELETE; still literal',
       E'backslash \\ and doubled '' UPDATE', "CREATE"";DROP",
       $$COPY; DELETE$$, $tag$TRUNCATE; CREATE$tag$;
/* CREATE; /* INSERT; */ DROP; */ -- UPDATE; DELETE
COMMIT;'''
        self.assertEqual(len(sql_statements(sql)), 3)
        assert_readonly_sql(self, sql)

    def test_writes_cannot_hide_after_select_comments_or_inside_with(self):
        for write in ('CREATE SCHEMA "owned"', 'DROP SCHEMA "owned" CASCADE',
                      'INSERT INTO docs VALUES (1)', 'UPDATE docs SET id=2',
                      'DELETE FROM docs', 'TRUNCATE docs', 'COPY docs TO STDOUT',
                      'ALTER TABLE docs ADD body text',
                      'WITH gone AS (DELETE FROM docs RETURNING *) SELECT * FROM gone',
                      'WITH source AS (SELECT 1) INSERT INTO docs SELECT * FROM source'):
            with self.subTest(write=write):
                with self.assertRaises(AssertionError):
                    assert_readonly_sql(self, "BEGIN READ ONLY; SELECT 'safe; CREATE'; /* safe */ " + write + "; COMMIT;")

    def test_readonly_transaction_is_required_and_malformed_input_fails_closed(self):
        for sql in ("BEGIN; SELECT 1; COMMIT;", "SELECT 1; COMMIT;",
                    "BEGIN READ ONLY; SET LOCAL transaction_read_only=off; COMMIT;",
                    "BEGIN READ ONLY; SET TRANSACTION READ WRITE; COMMIT;"):
            with self.subTest(sql=sql), self.assertRaises(AssertionError):
                assert_readonly_sql(self, sql)
        for sql in ("SELECT 'CREATE", 'SELECT "DROP', "SELECT $$DELETE", "/* UPDATE"):
            with self.subTest(sql=sql), self.assertRaises(ValueError):
                sql_statements(sql)


class ImportContractTests(unittest.TestCase):
    def test_import_does_not_run_cli_or_subprocess(self):
        out, err = io.StringIO(), io.StringIO()
        with blocked_processes(AssertionError("process attempted on import")) as calls:
            with mock.patch.object(sys, "argv", [str(RUNNER), "--not-a-cli-option"]):
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    module = import_runner()
        self.assertTrue(callable(module.validate_run_id))
        self.assertEqual(out.getvalue(), "")
        self.assertEqual(err.getvalue(), "")
        for call in calls:
            call.assert_not_called()


class PureContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with blocked_processes(AssertionError("process attempted on import")):
            cls.api = import_runner()

    def assertEqualFlag(self, result, expected):
        self.assertIsInstance(result, dict)
        self.assertIn("equal", result)
        self.assertIs(result["equal"], expected)

    def test_run_id_valid_and_boundary(self):
        for value in ("a", "trial01", "a_0", "a" * 32):
            with self.subTest(value=value):
                self.assertEqual(self.api.validate_run_id(value), value)

    def test_run_id_rejects_identifier_injection_and_invalid_forms(self):
        for value in ("", "1trial", "_trial", "Trial", "a" * 33, "a-b",
                      "a.b", "a b", "a\n", " a", "é", "a\x00b",
                      'a"; DROP SCHEMA public CASCADE;--', "a';SELECT 1;--",
                      "a/*comment*/", "a\\b"):
            with self.subTest(value=value):
                with self.assertRaises((ValueError, TypeError)):
                    self.api.validate_run_id(value)

    def test_sql_identifier_quoting(self):
        cases = {"docs": '"docs"', 'a"b': '"a""b"',
                 "O'Reilly": '"O\'Reilly"', "a.b": '"a.b"',
                 'x"; DROP TABLE docs;--': '"x""; DROP TABLE docs;--"'}
        for value, expected in cases.items():
            with self.subTest(value=value):
                self.assertEqual(self.api.quote_ident(value), expected)

    def test_sql_literal_quoting(self):
        cases = {"": "''", "O'Reilly": "'O''Reilly'", 'a"b': "'a\"b'",
                 "café 東京": "'café 東京'", "x'; DROP TABLE docs;--":
                 "'x''; DROP TABLE docs;--'"}
        for value, expected in cases.items():
            with self.subTest(value=value):
                self.assertEqual(self.api.quote_literal(value), expected)

    def test_corpus_seed_determinism_stable_ids_and_sentinels(self):
        rows = list(self.api.corpus_rows(64, 817))
        self.assertEqual(rows, list(self.api.corpus_rows(64, 817)))
        self.assertNotEqual(rows, list(self.api.corpus_rows(64, 818)))
        ids = [row[0] for row in rows]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertTrue(set(range(1, 65)).issubset(ids))
        self.assertTrue(all(isinstance(i, int) and i > 0 for i in ids))
        bodies = [row[1] for row in rows]
        self.assertTrue(all(body is None or isinstance(body, str) for body in bodies))
        self.assertIn(None, bodies)
        self.assertIn("", bodies)
        self.assertTrue(any(body and any(ord(c) > 127 for c in body) for body in bodies))
        self.assertTrue(any(body and len(body.split()) > 1 for body in bodies))
        # The contract intentionally does not prescribe how many sentinels exist.
        for word in ("rare", "common", "medium"):
            self.assertTrue(any(body and word in body for body in bodies), word)

    def test_corpus_rejects_negative_size(self):
        with self.assertRaises((ValueError, TypeError)):
            list(self.api.corpus_rows(-1, 1))

    def test_fingerprint_is_deterministic_sha256_and_accepts_iterable(self):
        rows = list(self.api.corpus_rows(23, 41))
        digest = self.api.corpus_fingerprint(rows)
        self.assertIsInstance(digest, str)
        self.assertRegex(digest, r"\A[0-9a-fA-F]{64}\Z")
        self.assertEqual(digest, self.api.corpus_fingerprint(iter(rows)))
        self.assertNotEqual(digest, self.api.corpus_fingerprint(list(self.api.corpus_rows(23, 42))))

    def test_fingerprint_distinguishes_null_empty_unicode_ids_and_multiplicity(self):
        samples = [[(1, None)], [(1, "")], [(1, "null")], [(1, "é")],
                   [(1, "e\u0301")], [(1, "東京")], [(2, "東京")],
                   [(1, "東京"), (1, "東京")], [(1, "a\nb")], [(1, "a"), (2, "b")]]
        hashes = [self.api.corpus_fingerprint(rows) for rows in samples]
        self.assertEqual(len(set(hashes)), len(samples))

    def test_ids_empty_equal_and_order_independent(self):
        self.assertEqualFlag(self.api.compare_ids([], []), True)
        self.assertEqualFlag(self.api.compare_ids([3, 1, 2, 1], [1, 2, 1, 3]), True)

    def test_ids_compare_full_multiset_not_counts_or_sets(self):
        for left, right in (([1, 2], [1, 3]), ([1, 1, 2], [1, 2, 2]),
                            ([1, 1], [1]), ([], [1]),
                            (list(range(1000)), list(range(999)) + [1001])):
            with self.subTest(left_size=len(left), right_size=len(right)):
                self.assertEqualFlag(self.api.compare_ids(left, right), False)

    def test_example_limit_must_not_truncate_equality_check(self):
        left = list(range(100))
        right = left[:-1] + [999]
        for limit in (0, 1, 3):
            with self.subTest(limit=limit):
                self.assertEqualFlag(self.api.compare_ids(left, right, limit=limit), False)
                self.assertEqualFlag(self.api.compare_ids(left, left, limit=limit), True)

    def test_scores_exact_and_missing_ids(self):
        self.assertEqualFlag(self.api.compare_scores({}, {}, 0.0, 0.0), True)
        self.assertEqualFlag(self.api.compare_scores({1: 1.25, 2: -2.5}, {2: -2.5, 1: 1.25}, 0.0, 0.0), True)
        for left, right in (({1: 1.0}, {2: 1.0}), ({1: 1.0}, {}),
                            ({}, {1: 1.0}), ({1: 1.0}, {1: 1.0001})):
            with self.subTest(left=left, right=right):
                self.assertEqualFlag(self.api.compare_scores(left, right, 0.0, 0.0), False)

    def test_score_absolute_tolerance_boundary(self):
        for value, expected in ((0.25, True), (math.nextafter(0.25, math.inf), False)):
            self.assertEqualFlag(self.api.compare_scores({1: 0.0}, {1: value}, 0.25, 0.0), expected)

    def test_score_relative_tolerance_boundary(self):
        # Larger right-hand reference avoids conflating asymmetric/isclose rules.
        # |7-8| = 0.125 * 8; both conventional definitions agree here.
        self.assertEqualFlag(self.api.compare_scores({1: 7.0}, {1: 8.0}, 0.0, 0.125), True)
        self.assertEqualFlag(self.api.compare_scores({1: math.nextafter(7.0, -math.inf)}, {1: 8.0}, 0.0, 0.125), False)

    def test_scores_reject_nonfinite_on_either_side(self):
        for value in (math.nan, math.inf, -math.inf):
            for left, right in (({1: value}, {1: 0.0}), ({1: 0.0}, {1: value}),
                                ({1: value}, {1: value}), ({1: value}, {})):
                with self.subTest(value=value, left=left, right=right):
                    try:
                        result = self.api.compare_scores(left, right, 0.1, 0.1)
                    except (ValueError, TypeError):
                        continue
                    # A structured rejected/mismatch result is also legitimate.
                    self.assertEqualFlag(result, False)

    def test_scores_reject_negative_tolerances_even_for_empty_inputs(self):
        for atol, rtol in ((-1.0, 0.0), (0.0, -1.0), (-0.1, -0.1)):
            for values in ({}, {1: 1.0}):
                with self.subTest(atol=atol, rtol=rtol, values=values):
                    with self.assertRaises((ValueError, TypeError)):
                        self.api.compare_scores(values, values, atol, rtol)


class CliGuardTests(unittest.TestCase):
    def invoke(self, arguments, effect):
        out, err = io.StringIO(), io.StringIO()
        with blocked_processes(effect) as calls:
            with mock.patch.object(sys, "argv", [str(RUNNER)] + arguments):
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    try:
                        runpy.run_path(str(RUNNER), run_name="__main__")
                        code = 0
                    except SystemExit as exc:
                        code = exc.code
        return code, out.getvalue() + err.getvalue(), calls

    @staticmethod
    def required_args(output):
        return ["--plumb-service", "parity_plumb", "--tin-service", "parity_tin",
                "--run-id", "test001", "--out-dir", str(output)]

    def assertRejectedBeforeProcess(self, args):
        code, text, calls = self.invoke(args, AssertionError("invalid CLI launched subprocess"))
        self.assertNotIn(code, (None, 0))
        self.assertNotIn(SECRET, text)
        for call in calls:
            call.assert_not_called()

    def test_missing_required_arguments_cannot_launch_db_command(self):
        self.assertRejectedBeforeProcess([])
        self.assertRejectedBeforeProcess(["compare"])
        with tempfile.TemporaryDirectory() as root:
            args = self.required_args(Path(root) / "report")
            for option in ("--plumb-service", "--tin-service", "--run-id", "--out-dir"):
                index = args.index(option)
                with self.subTest(option=option):
                    self.assertRejectedBeforeProcess(["compare"] + args[:index] + args[index + 2:])

    def test_prepare_without_explicit_write_flag_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertRejectedBeforeProcess(["prepare"] + self.required_args(Path(root) / "report"))

    def test_credentials_and_connection_strings_are_not_cli_options(self):
        with tempfile.TemporaryDirectory() as root:
            args = ["compare"] + self.required_args(Path(root) / "report")
            for option in ("--password", "--dsn", "--plumb-dsn", "--tin-dsn"):
                with self.subTest(option=option):
                    self.assertRejectedBeforeProcess(args + [option, SECRET])
            for value in ("postgresql://user:password@host/db", "host=localhost password=x",
                          "a; SELECT 1", "a\nother"):
                altered = list(args)
                altered[altered.index("--tin-service") + 1] = value
                with self.subTest(service=value):
                    self.assertRejectedBeforeProcess(altered)

    def test_main_parser_errors_are_fixed_and_never_launch_process_or_write(self):
        with blocked_processes(AssertionError("process attempted on import")):
            api = import_runner()
        bad_arguments = [
            ["--password", SECRET], ["--password=" + SECRET],
            ["--" + SECRET], ["--rows", SECRET], ["--atol", SECRET],
            [SECRET], ["--tin-label", SECRET],
            ["--tin-label", "postgresql://user:" + SECRET + "@host/db"],
            ["--tin-label", "host=localhost password=" + SECRET],
            ["--tin-label", "unverified " + SECRET], ["--rows"],
        ]
        for extra in bad_arguments:
            with self.subTest(extra=extra), tempfile.TemporaryDirectory() as root:
                output = Path(root) / "report"
                stdout, stderr = io.StringIO(), io.StringIO()
                with blocked_processes(AssertionError("invalid CLI launched process")) as calls:
                    # Error output must not print even a caller-supplied prog name.
                    with mock.patch.object(sys, "argv", [SECRET]), \
                         contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                        with self.assertRaises(SystemExit) as caught:
                            api.main(self.required_args(output) + extra)
                self.assertEqual(caught.exception.code, 2)
                self.assertEqual(stdout.getvalue(), "")
                self.assertEqual(stderr.getvalue(), "Invalid arguments; see --help. No connections attempted.\n")
                self.assertNotIn(SECRET, stdout.getvalue())
                self.assertNotIn(SECRET, stderr.getvalue())
                self.assertFalse(output.exists())
                for call in calls:
                    call.assert_not_called()

    def test_main_missing_required_arguments_use_same_fixed_error(self):
        with blocked_processes(AssertionError("invalid CLI launched process")) as calls:
            api = import_runner()
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as caught:
                    api.main(["--password", SECRET])
        self.assertEqual(caught.exception.code, 2)
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(stderr.getvalue(), "Invalid arguments; see --help. No connections attempted.\n")
        for call in calls:
            call.assert_not_called()

    def test_provenance_categories_and_default_are_fixed_in_reports(self):
        for label in (None, "unverified", "public-lead", "planetscale-tin"):
            with self.subTest(label=label), tempfile.TemporaryDirectory() as root:
                output = Path(root) / "report"
                args = self.required_args(output)
                if label is not None:
                    args += ["--tin-label", label]
                code, text, calls = self.invoke(args, subprocess.TimeoutExpired(
                    "psql", 120, output=SECRET, stderr=SECRET))
                self.assertEqual(code, 1)
                self.assertEqual(calls[0].call_count, 2)
                for call in calls[1:]:
                    call.assert_not_called()
                report = json.loads((output / "report.json").read_text(encoding="utf-8"))
                self.assertEqual(report["tin_label"], label or "unverified")
                self.assertEqual(report["error"], "endpoint_preflight_failed")
                self.assertIn("operator assertion", " ".join(report["caveats"]))
                self.assertNotIn(SECRET, text)
                for path in output.iterdir():
                    self.assertNotIn(SECRET, path.read_text(encoding="utf-8"))

    def test_invalid_schema_run_id_and_identical_services_are_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            args = ["compare"] + self.required_args(Path(root) / "report")
            for option, value in (("--run-id", 'x"; DROP SCHEMA public;--'),
                                  ("--tin-service", "parity_plumb")):
                altered = list(args)
                altered[altered.index(option) + 1] = value
                self.assertRejectedBeforeProcess(altered)

    def assertFailedPreflightIsSafe(self, command):
        attempts = []

        def fail_process(*args, **kwargs):
            attempts.append((args, kwargs))
            actual_command = args[0] if args else kwargs.get("args", [])
            raise subprocess.CalledProcessError(2, actual_command, output=SECRET,
                                                 stderr="postgresql://user:" + SECRET + "@host/db")

        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "report"
            args = self.required_args(output)
            if command:
                args.insert(0, command)
            if command == "prepare":
                args.append("--allow-fixture-writes")
            code, text, _ = self.invoke(args, fail_process)
            self.assertNotIn(code, (None, 0))
            self.assertTrue(attempts, "CLI must reach mocked preflight with valid service arguments")
            self.assertNotIn(SECRET, text)
            self.assertNotIn("Traceback (most recent call last)", text)
            for report in Path(root).rglob("*"):
                if report.is_file():
                    self.assertNotIn(SECRET.encode(), report.read_bytes(), str(report))
            for positional, keywords in attempts:
                # Endpoint.execute sends actual SQL on stdin, never argv/repr.
                self.assertIn("input", keywords)
                assert_readonly_sql(self, keywords["input"])
                self.assertNotIn(SECRET, keywords["input"])
                self.assertNotIn(SECRET, repr(positional))

    def test_compare_preflight_failure_does_not_write_or_forward_connection_error(self):
        self.assertFailedPreflightIsSafe("compare")

    def test_implicit_compare_preflight_failure_never_creates_or_drops(self):
        self.assertFailedPreflightIsSafe(None)

    def test_prepare_first_preflight_failure_prevents_all_fixture_writes(self):
        self.assertFailedPreflightIsSafe("prepare")


class CopyProtocolTests(unittest.TestCase):
    """Inspect real prepare/execute assembly without connecting to a database."""

    def setUp(self):
        with blocked_processes(AssertionError("process attempted on import")):
            self.api = import_runner()

    def check_prepare(self, failure=None):
        api = self.api
        ep = api.Endpoint("plumb", "parity_plumb")
        canonical = list(api.corpus_rows(3, 1729))
        paths = []

        def transport(command, **kwargs):
            sql = kwargs["input"]
            lines = sql.splitlines()
            copy_lines = [line for line in lines if line.startswith("\\copy ")]
            self.assertEqual(len(copy_lines), 1)
            match = re.fullmatch(
                r'''\\copy "plumb_parity_test001"\."docs" \(id,body\) FROM '(/[A-Za-z0-9_./-]+)' WITH \(FORMAT csv, NULL E'\\\\N'\)''',
                copy_lines[0])
            self.assertIsNotNone(match)
            path = Path(match.group(1))
            paths.append(path)
            self.assertTrue(path.is_file(), "CSV must remain present throughout execute")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with path.open(encoding="utf-8", newline="") as stream:
                raw = stream.read()
            # CSV reader handles quoting; only the unquoted sentinel is SQL NULL.
            records = list(csv.reader(io.StringIO(raw, newline="")))
            raw_lines = raw.splitlines()
            self.assertEqual(len(records), len(raw_lines))
            decoded = [(int(record[0]), None if line.split(",", 1)[1] == r"\N" else record[1])
                       for record, line in zip(records, raw_lines)]
            self.assertEqual(decoded, canonical)
            self.assertEqual([api.canonical_row(row) for row in decoded],
                             [api.canonical_row(row) for row in canonical])
            self.assertNotIn(r"\.", lines)
            self.assertNotIn(r"\.", raw_lines)
            self.assertNotIn("FROM STDIN", sql)
            self.assertNotIn("café", sql)
            self.assertNotIn("literal", sql)
            self.assertEqual(lines.count("BEGIN;"), 1)
            self.assertEqual(lines.count("COMMIT;"), 1)
            self.assertEqual(lines[0], "BEGIN;")
            self.assertEqual(lines[-1], "COMMIT;")
            self.assertLess(sql.index("CREATE TABLE"), sql.index("\\copy "))
            self.assertLess(sql.index("\\copy "), sql.index("CREATE INDEX"))
            self.assertLess(sql.index("CREATE INDEX"), sql.index("INSERT INTO"))
            self.assertEqual(command, ["psql", "-X", "-w", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=1"])
            if failure == "timeout":
                raise subprocess.TimeoutExpired(command, 130, output=SECRET, stderr=SECRET)
            return subprocess.CompletedProcess(command, 2 if failure else 0,
                                               stdout=SECRET if failure else "",
                                               stderr=SECRET if failure else "")

        stdout, stderr = io.StringIO(), io.StringIO()
        with blocked_processes(AssertionError("unexpected process")), \
             mock.patch.object(subprocess, "run", side_effect=transport) as process, \
             mock.patch.object(ep, "execute", wraps=ep.execute) as execute, \
             contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            marker = api.make_marker("plumb", "test001", 3, 1729)
            if failure:
                expected = "client_timeout" if failure == "timeout" else "authentication_failed"
                with self.assertRaisesRegex(api.HarnessError, "^" + expected + "$"):
                    ep.prepare("plumb_parity_test001", marker, 3, 1729)
            else:
                ep.prepare("plumb_parity_test001", marker, 3, 1729)
            execute.assert_called_once()
            self.assertFalse(execute.call_args.kwargs["readonly"])
            process.assert_called_once()
        self.assertEqual(stdout.getvalue() + stderr.getvalue(), "")
        self.assertEqual(len(paths), 1)
        self.assertFalse(paths[0].exists(), "CSV must be removed even when execute raises")

    def test_file_copy_preserves_canonical_rows_and_cleans_up_success(self):
        self.check_prepare()

    def test_file_copy_cleans_up_server_error_and_client_timeout(self):
        for failure in ("server", "timeout"):
            with self.subTest(failure=failure):
                self.check_prepare(failure)

    def test_unsafe_temp_paths_never_enter_psql_and_are_removed(self):
        original = tempfile.NamedTemporaryFile
        for suffix in ("\n\\! echo injected", "\r", "'", "\\", ";", " ", ":variable", "`cmd`", "é"):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as root:
                directory = Path(root) / ("unsafe" + suffix)
                directory.mkdir()
                ep = self.api.Endpoint("plumb", "parity_plumb")
                with mock.patch.object(tempfile, "NamedTemporaryFile",
                                       side_effect=lambda **kw: original(dir=directory, **kw)), \
                     mock.patch.object(ep, "execute") as execute:
                    with self.assertRaisesRegex(self.api.HarnessError, "^unsafe_copy_file_path$"):
                        ep.prepare("plumb_parity_test001", {}, 3, 1729)
                execute.assert_not_called()
                self.assertEqual(list(directory.iterdir()), [])

    def test_tempfile_creation_error_is_fixed_and_does_not_execute(self):
        ep = self.api.Endpoint("plumb", "parity_plumb")
        with mock.patch.object(tempfile, "NamedTemporaryFile", side_effect=OSError(SECRET)), \
             mock.patch.object(ep, "execute") as execute:
            with self.assertRaisesRegex(self.api.HarnessError, "^copy_file_failed$"):
                ep.prepare("plumb_parity_test001", {}, 3, 1729)
        execute.assert_not_called()

    def test_preflight_locale_uses_pg_database_not_removed_settings(self):
        for role in ("plumb", "tin"):
            ep = self.api.Endpoint(role, "parity_" + role)
            payload = {"am": True, "operator": True, "lc_ctype": "C.UTF-8", "lc_collate": "C"}
            with blocked_processes(AssertionError("unexpected process")), \
                 mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                     ["psql"], 0, stdout=json.dumps(payload), stderr="")) as process:
                result = ep.preflight("plumb_parity_test001")
            sql = process.call_args.kwargs["input"]
            assert_readonly_sql(self, sql)
            for key, column in (("lc_ctype", "datctype"), ("lc_collate", "datcollate")):
                self.assertIn(f"'{key}', (SELECT {column} FROM pg_database WHERE datname=current_database())", sql)
                self.assertNotIn(f"current_setting('{key}')", sql)
                self.assertEqual(result[key], payload[key])
            self.assertEqual(ep.metadata, payload)


class IndexMetadataTests(unittest.TestCase):
    def setUp(self):
        with blocked_processes(AssertionError("process attempted on import")):
            self.api = import_runner()

    def structure_payload(self, role):
        return {"tables": 2, "columns": [["id", "bigint", True], ["body", "text", False]],
                "primary_key": True, "search_index": True,
                "search_indexes": search_index_metadata(role)}

    def test_structure_records_stably_ordered_native_index_metadata_readonly(self):
        for role in ("plumb", "tin"):
            with self.subTest(role=role):
                ep = self.api.Endpoint(role, "parity_" + role)
                with blocked_processes(AssertionError("unexpected process")), \
                     mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                         ["psql"], 0, stdout=json.dumps(self.structure_payload(role)), stderr="")) as process:
                    indexes = ep.check_structure("plumb_parity_test001")
                self.assertEqual(indexes, search_index_metadata(role))
                process.assert_called_once()
                sql = process.call_args.kwargs["input"]
                assert_readonly_sql(self, sql)
                for fragment in ("pg_get_indexdef(i.indexrelid)", 'ORDER BY ix.relname COLLATE "C"',
                                 'ORDER BY option COLLATE "C"', "unnest(ix.reloptions)",
                                 "'indnkeyatts', i.indnkeyatts", "ORDER BY k.ordinality",
                                 "op.opcname", "ns.nspname", "am.amname='" + role + "'",
                                 "i.indisvalid AND i.indisready", "i.indpred IS NULL",
                                 "i.indexprs IS NULL", "i.indnkeyatts=1", "i.indkey::text='2'"):
                    self.assertIn(fragment, sql)
                # Metadata is descriptive, not an unsupported opclass identity gate.
                self.assertNotIn("op.opcname=", sql)

    def test_structure_requires_exactly_one_valid_native_body_index(self):
        for indexes, valid_shape in (([], True), (search_index_metadata("tin") * 2, True),
                                     (search_index_metadata("tin"), False)):
            with self.subTest(indexes=indexes, valid_shape=valid_shape):
                ep = self.api.Endpoint("tin", "parity_tin")
                payload = self.structure_payload("tin")
                payload.update(search_indexes=indexes, search_index=valid_shape)
                with mock.patch.object(ep, "json", return_value=(payload, 0)):
                    with self.assertRaisesRegex(self.api.HarnessError, "^fixture_structure_mismatch$"):
                        ep.check_structure("plumb_parity_test001")

    def test_verify_fixture_keeps_original_marker_and_row_hash_contract(self):
        ep = self.api.Endpoint("tin", "parity_tin")
        marker = self.api.make_marker("tin", "test001", 3, 1729)
        original = dict(marker)
        fingerprint = {"rows": marker["expected_rows"], "sha256": marker["sha256"]}
        with mock.patch.object(ep, "check_structure", return_value=search_index_metadata("tin")), \
             mock.patch.object(ep, "marker", return_value=marker), \
             mock.patch.object(ep, "fingerprint", return_value=fingerprint):
            actual = self.api.verify_fixture(ep, "plumb_parity_test001", marker)
        self.assertEqual(actual, {**fingerprint, "search_indexes": search_index_metadata("tin")})
        self.assertEqual(marker, original)
        self.assertEqual(set(fingerprint), {"rows", "sha256"})
        self.assertEqual(self.api.validate_marker(marker, "tin", "test001"), original)


class EndpointOrchestrationTests(unittest.TestCase):
    """Exercise real main/run/comparison and real Endpoint SQL/transport handling."""

    def setUp(self):
        with blocked_processes(AssertionError("process attempted on import")):
            self.api = import_runner()

    def scenario(self, action="compare", *, preflight_failure=None,
                 prepare_failure=None, missing_score=False, with_scores=False,
                 result_count=1, drift=False, score_records=None, rank_records=None,
                 index_drift=None):
        api = self.api
        events, submissions, fingerprints = [], [], {"plumb": 0, "tin": 0}
        markers = {role: api.make_marker(role, "test001", 3, 1729)
                   for role in ("plumb", "tin")}

        def transport(command, **kwargs):
            # All real Endpoint.preflight/prepare calls pass through here. No psql.
            role = {"parity_plumb": "plumb", "parity_tin": "tin"}[kwargs["env"]["PGSERVICE"]]
            sql = kwargs["input"]
            submissions.append((role, sql))
            self.assertEqual(command[0], "psql")
            if sql.startswith("BEGIN READ ONLY;"):
                assert_readonly_sql(self, sql.replace("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ", ""))
                if "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) " in sql:
                    events.append(("measure", role))
                    payload = [{"Planning Time": 0.1, "Execution Time": 0.2,
                                "Plan": {"Node Type": "Index Scan", "Index Name": "docs_search_idx"}}]
                else:
                    self.assertIn("has_database_privilege(current_database(), 'CREATE')", sql)
                    events.append(("preflight", role))
                    if role == "tin" and preflight_failure:
                        if preflight_failure == "timeout":
                            raise subprocess.TimeoutExpired(command, 130, output=SECRET, stderr=SECRET)
                        return subprocess.CompletedProcess(command, 2, stdout=SECRET,
                            stderr="connection to server failed: postgresql://user:" + SECRET + "@host/db")
                    payload = {"identity": [role, "127.0.0.1", 5432, 1],
                               "am": True, "operator": True,
                               "schema_exists": action == "compare",
                               "can_create_schema": True, "in_recovery": False,
                               "full_score": not missing_score, "highlight": True}
                return subprocess.CompletedProcess(command, 0, stdout=json.dumps(payload), stderr="")
            events.append(("prepare", role))
            self.assertTrue(sql.startswith("BEGIN;\n"))
            self.assertIn('CREATE SCHEMA "plumb_parity_test001";', sql)
            self.assertTrue(sql.endswith("COMMIT;\n"))
            if role == prepare_failure:
                return subprocess.CompletedProcess(command, 2, stdout=SECRET,
                                                   stderr="connection refused: " + SECRET)
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        def marker(ep, schema):
            events.append(("marker", ep.role))
            return dict(markers[ep.role])

        def check_structure(ep, schema):
            indexes = search_index_metadata(ep.role)
            if index_drift and ep.role == "tin" and fingerprints[ep.role] == 1:
                indexes[0].update(index_drift)
            return indexes

        def fingerprint(ep, schema, max_rows):
            events.append(("fingerprint", ep.role))
            fingerprints[ep.role] += 1
            value = {"rows": markers[ep.role]["expected_rows"], "sha256": markers[ep.role]["sha256"]}
            if drift and ep.role == "tin" and fingerprints[ep.role] == 2:
                value["sha256"] = "0" * 64
            return value

        def records(ep, select):
            events.append(("records", ep.role))
            if "full_score" in select:
                self.assertFalse(missing_score, "missing capability must not execute score SQL")
                if "score_bits" in select:
                    events.append(("scores", ep.role))
                    self.assertRegex(select, r"LIMIT 3\Z")
                    rows = score_records if score_records is not None else [
                        {"id": ident, "score": 1.0, "score_bits": "3f800000"}
                        for ident in range(1, result_count + 1)]
                else:
                    events.append(("rank", ep.role))
                    self.assertIn("ORDER BY score DESC, id ASC LIMIT 20", select)
                    rows = rank_records if rank_records is not None else [
                        {"id": ident, "score": 1.0} for ident in range(1, result_count + 1)]
                return rows, 0.5
            self.assertRegex(select, r"LIMIT 3\Z")  # cap + 1, not a silently truncated cap
            return [{"id": ident} for ident in range(1, result_count + 1)], 0.5

        out, err = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory() as root, contextlib.ExitStack() as stack:
            output = Path(root) / "report"
            cases = Path(root) / "cases.json"
            cases.write_text(json.dumps([{"name": "common", "query": "common"}]), encoding="utf-8")
            args = [action] + CliGuardTests.required_args(output) + [
                "--rows", "3", "--max-results", "2", "--samples", "1", "--cases-json", str(cases)]
            if action == "prepare":
                args.append("--allow-fixture-writes")
            if with_scores:
                args.append("--with-scores")
            blocked = stack.enter_context(blocked_processes(AssertionError("unexpected subprocess API")))
            # Override only run, the API Endpoint.execute actually calls.
            process = stack.enter_context(mock.patch.object(subprocess, "run", side_effect=transport))
            stack.enter_context(mock.patch.object(api.Endpoint, "check_structure", autospec=True, side_effect=check_structure))
            stack.enter_context(mock.patch.object(api.Endpoint, "marker", autospec=True, side_effect=marker))
            stack.enter_context(mock.patch.object(api.Endpoint, "fingerprint", autospec=True, side_effect=fingerprint))
            stack.enter_context(mock.patch.object(api.Endpoint, "records", autospec=True, side_effect=records))
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = api.main(args)
            self.assertTrue(process.called)
            # Audit outside main: the runner catches Exception, including failed
            # assertions raised inside mocks, so those alone are not an oracle.
            for _, sql in submissions:
                if action == "compare" or sql.startswith("BEGIN READ ONLY;"):
                    assert_readonly_sql(self, sql.replace("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ", ""))
                else:
                    self.assertTrue(sql.startswith("BEGIN;\n"))
                    self.assertIn('CREATE SCHEMA "plumb_parity_test001";', sql)
                    self.assertTrue(sql.endswith("COMMIT;\n"))
            for unexpected in blocked:
                unexpected.assert_not_called()
            report = json.loads((output / "report.json").read_text(encoding="utf-8"))
            summary = (output / "summary.md").read_text(encoding="utf-8")
            text = out.getvalue() + err.getvalue() + summary
            self.assertNotIn(SECRET, text)
            self.assertNotIn("Traceback (most recent call last)", text)
            for path in output.rglob("*"):
                if path.is_file():
                    self.assertNotIn(SECRET.encode(), path.read_bytes(), str(path))
        return code, report, text, events, submissions

    def assertIncomplete(self, result):
        code, report, text, _, _ = result
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "incomplete")
        self.assertIn("**Status: incomplete**", text)
        self.assertNotIn("Parity runner status: passed", text)
        self.assertNotIn("**Status: passed**", text)

    def test_provider_specific_index_definitions_are_recorded_not_compared_across_roles(self):
        code, report, _, _, _ = self.scenario()
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "passed")
        for role in ("plumb", "tin"):
            before = report["fingerprints_before"][role]
            self.assertEqual(before, report["fingerprints_after"][role])
            self.assertEqual(before["search_indexes"], search_index_metadata(role))
            self.assertEqual(before["rows"], 11)
            self.assertEqual(before["sha256"], report["markers"][role]["sha256"])
            self.assertNotIn("search_indexes", report["markers"][role])
        self.assertNotEqual(report["fingerprints_before"]["plumb"]["search_indexes"],
                            report["fingerprints_before"]["tin"]["search_indexes"])

    def test_same_endpoint_index_metadata_drift_is_incomplete_with_unchanged_corpus(self):
        for changed in ({"definition": search_index_metadata("tin")[0]["definition"] + " WITH (example=1)"},
                        {"reloptions": ["example=1"]}, {"indnkeyatts": 2},
                        {"opclasses": [{"schema": "tin", "name": "another_text_ops"}]}):
            with self.subTest(changed=changed):
                result = self.scenario(index_drift=changed)
                self.assertIncomplete(result)
                report = result[1]
                before, after = report["fingerprints_before"]["tin"], report["fingerprints_after"]["tin"]
                self.assertEqual(after["error"], "fixture_index_metadata_changed")
                self.assertEqual(after["rows"], before["rows"])
                self.assertEqual(after["sha256"], before["sha256"])
                self.assertEqual(before["search_indexes"], search_index_metadata("tin"))
                for field, value in changed.items():
                    self.assertEqual(after["search_indexes"][0][field], value)
                self.assertEqual(report["cases"][0]["status"], "passed")
                self.assertEqual(report["fingerprints_before"]["plumb"], report["fingerprints_after"]["plumb"])

    def test_both_readonly_preflights_finish_before_any_prepare(self):
        code, report, _, events, submissions = self.scenario("prepare")
        self.assertEqual(code, 0)
        self.assertEqual(report["preparation_status"], "prepared")
        self.assertEqual(events[:4], [("preflight", "plumb"), ("preflight", "tin"),
                                     ("prepare", "plumb"), ("prepare", "tin")])
        self.assertEqual(len(submissions), 4)
        for _, sql in submissions[:2]:
            assert_readonly_sql(self, sql)
        self.assertEqual([report["preparation"][role]["status"] for role in ("plumb", "tin")],
                         ["committed", "committed"])

    def test_first_preflight_success_second_failure_means_zero_writes(self):
        result = self.scenario("prepare", preflight_failure="connection")
        self.assertIncomplete(result)
        _, report, _, events, submissions = result
        self.assertEqual(events, [("preflight", "plumb"), ("preflight", "tin")])
        self.assertEqual(len(submissions), 2)
        self.assertNotIn("preparation", report)
        self.assertEqual(report["error"], "endpoint_preflight_failed")
        self.assertTrue(report["endpoints"]["plumb"]["am"])
        for _, sql in submissions:
            assert_readonly_sql(self, sql)

    def test_partial_prepare_is_incomplete_and_preserves_first_commit_without_cleanup(self):
        result = self.scenario("prepare", prepare_failure="tin")
        self.assertIncomplete(result)
        _, report, _, events, submissions = result
        self.assertEqual(events, [("preflight", "plumb"), ("preflight", "tin"),
                                  ("prepare", "plumb"), ("prepare", "tin")])
        self.assertEqual(len(submissions), 4, "no retry, verification, or cleanup after partial prepare")
        self.assertEqual(report["preparation_status"], "partial_preparation")
        self.assertEqual(report["preparation"]["plumb"]["status"], "committed")
        self.assertEqual(report["preparation"]["tin"]["status"], "failed_or_commit_unknown")
        self.assertIn("No cleanup attempted", report["preservation_note"])
        for _, sql in submissions:
            self.assertNotRegex(sql, re.compile(r"\b(?:DROP|TRUNCATE|DELETE)\b", re.I))

    def test_selected_missing_score_capability_cannot_pass(self):
        result = self.scenario(missing_score=True, with_scores=True)
        self.assertIncomplete(result)
        report = result[1]
        case = report["cases"][0]
        self.assertTrue(case["matches"]["equal"])
        self.assertEqual(case["status"], "incomplete")
        self.assertEqual([case["endpoints"][role]["score_error"] for role in ("plumb", "tin")],
                         ["missing_full_score_capability"] * 2)
        self.assertEqual(report["selected_suite_errors"][0]["suite"], "scores")

    def assertInvalidScore(self, rows, error):
        result = self.scenario(with_scores=True, score_records=rows)
        self.assertIncomplete(result)
        case = result[1]["cases"][0]
        self.assertEqual(case["status"], "incomplete")
        self.assertTrue(case["matches"]["equal"])
        self.assertNotIn("scores", case)
        self.assertNotIn("rank", case)
        for role in ("plumb", "tin"):
            self.assertEqual(case["endpoints"][role]["score_error"], error)
            self.assertNotIn(("rank", role), result[3])

    def test_score_bits_must_be_exactly_eight_hex_characters(self):
        for bits in (None, True, 12345678, [], {}, "", "3f80000", "3f8000000",
                     "3g800000", "3f800000\n", " 3f800000", SECRET):
            with self.subTest(bits=bits):
                self.assertInvalidScore([{"id": 1, "score": 1.0, "score_bits": bits}],
                                        "invalid_score_bits")

    def test_score_ids_reject_boolean_float_string_and_unhashable_aliases(self):
        for ident in (True, False, 1.0, "1", SECRET, None, [], {}):
            with self.subTest(ident=ident):
                self.assertInvalidScore([{"id": ident, "score": 1.0, "score_bits": "3f800000"}],
                                        "invalid_score_ids")

    def test_score_values_are_not_coerced_from_bool_or_text(self):
        for value in (True, False, "1.0", SECRET, None, [], {}, math.nan, math.inf, -math.inf):
            with self.subTest(value=value):
                self.assertInvalidScore([{"id": 1, "score": value, "score_bits": "3f800000"}],
                                        "invalid_score_value")

    def test_duplicate_missing_or_changed_score_ids_remain_incomplete(self):
        good = {"id": 1, "score": 1.0, "score_bits": "3f800000"}
        for rows in ([], [good, good], [dict(good, id=2)]):
            with self.subTest(rows=rows):
                self.assertInvalidScore(rows, "score_id_set_changed")

    def test_invalid_rank_ids_are_not_published_or_allowed_to_alias_integers(self):
        for ident in (True, 1.0, "1", SECRET, None, [], {}):
            with self.subTest(ident=ident):
                result = self.scenario(with_scores=True, rank_records=[{"id": ident, "score": 1.0}])
                self.assertIncomplete(result)
                case = result[1]["cases"][0]
                self.assertNotIn("rank", case)
                for role in ("plumb", "tin"):
                    self.assertEqual(case["endpoints"][role]["score_error"], "invalid_rank_result")

    def test_valid_score_wire_values_and_native_rank_can_pass(self):
        for bits in ("3f800000", "3F800000"):
            with self.subTest(bits=bits):
                code, report, _, events, _ = self.scenario(with_scores=True, score_records=[
                    {"id": 1, "score": 1.0, "score_bits": bits}])
                self.assertEqual(code, 0)
                self.assertEqual(report["status"], "passed")
                case = report["cases"][0]
                self.assertTrue(case["scores"]["equal"])
                self.assertTrue(case["scores"]["float4_wire_bits_equal"])
                self.assertEqual(case["rank"]["ids"], {"plumb": [1], "tin": [1]})
                self.assertEqual(report["selected_suite_errors"], [])
                for role in ("plumb", "tin"):
                    self.assertIn(("scores", role), events)
                    self.assertIn(("rank", role), events)

    def test_unselected_missing_score_capability_does_not_block_match_suite(self):
        code, report, _, _, _ = self.scenario(missing_score=True)
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["selected_suite_errors"], [])

    def test_cap_plus_one_results_are_incomplete_not_equal_truncated_pass(self):
        result = self.scenario(result_count=3)
        self.assertIncomplete(result)
        case = result[1]["cases"][0]
        self.assertEqual(case["status"], "incomplete")
        for role in ("plumb", "tin"):
            self.assertEqual(case["endpoints"][role]["returned_rows"], 3)
            self.assertEqual(case["endpoints"][role]["match_error"], "result_cap_exceeded")
        self.assertNotIn("matches", case)

    def test_exact_cap_results_can_pass(self):
        code, report, _, _, _ = self.scenario(result_count=2)
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "passed")
        self.assertTrue(report["cases"][0]["matches"]["equal"])

    def test_after_comparison_corpus_drift_overrides_equal_matches(self):
        result = self.scenario(drift=True)
        self.assertIncomplete(result)
        report = result[1]
        self.assertTrue(report["cases"][0]["matches"]["equal"])
        self.assertIn("sha256", report["fingerprints_before"]["tin"])
        self.assertEqual(report["fingerprints_after"]["tin"]["error"], "corpus_fingerprint_mismatch")
        self.assertEqual(report["fingerprints_before"]["plumb"], report["fingerprints_after"]["plumb"])

    def test_connection_stderr_and_timeout_output_secrets_never_reach_logs_or_reports(self):
        for action in ("prepare", "compare"):
            for failure in ("connection", "timeout"):
                with self.subTest(action=action, failure=failure):
                    result = self.scenario(action, preflight_failure=failure)
                    self.assertIncomplete(result)
                    self.assertEqual(result[3], [("preflight", "plumb"), ("preflight", "tin")])
                    expected = "client_timeout" if failure == "timeout" else "authentication_failed"
                    self.assertEqual(result[1]["endpoints"]["tin"]["error"], expected)


if __name__ == "__main__":
    unittest.main()
