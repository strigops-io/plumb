# Two-instance parity runner contract

One explicitly named libpq service is Plumb, the other is TIN. Never substitute an
endpoint or infer that an extension named tin proves it is hosted PlanetScale TIN.
Local public Lead tests validate the harness, not real hosted TIN parity.

## CLI

```sh
python3 benches/parity.py prepare --plumb-service plumb_local --tin-service tin_eval \
  --run-id trial01 --rows 1000 --out-dir /new/path/prepare-report \
  --allow-fixture-writes --tin-label planetscale-tin
python3 benches/parity.py compare --plumb-service plumb_local --tin-service tin_eval \
  --run-id trial01 --out-dir /new/path/parity-report --samples 3 \
  --with-scores --with-highlights --tin-label planetscale-tin
```

Default action is compare/read-only. prepare is a separately explicit command and
requires --allow-fixture-writes. Both services, run ID and new output directory are
required. Do not accept passwords/DSNs as CLI arguments. Use libpq pg_service.conf,
PGSERVICEFILE and .pgpass/PGPASSFILE; retain configured TLS and never downgrade it.
Require simple service names, no connection strings. Child psql uses PGSERVICE in
environment, -X -w and ON_ERROR_STOP. Remove ambient routing PGHOST/PGPORT/PGDATABASE/
PGUSER/PGOPTIONS/PGSERVICE so another target cannot silently override the service.
Credential/TLS settings remain in libpq config, never captured in reports, commands
or exception traces. Reports identify roles, versions and non-secret metadata.
Raw connection stderr is not archived (may expose connection/credential material).

Both endpoints must pass read-only preflight before any preparation writes. Require
canonical AM and operator/function capabilities; optional score/highlight suites
must report missing/failed capability as an incomplete selected suite, never PASS.
Refuse identical endpoint identities (database + server address/port + postmaster
start time), and identical service names. Identity is an accident guard, not proof
against proxies/aliases. Never create extensions/databases or change privileges.

## Fixture

Schema `plumb_parity_<run_id>` (run ID lowercase letters/digits/underscore, starts
with letter, <=32 chars). Same permanent docs table `(id bigint PRIMARY KEY, body
text)` and deterministic corpus on both endpoints. Plumb uses AM plumb and operator
pg_catalog.~~>; TIN uses AM tin and operator pg_catalog.==>. Function schemas follow
the adapter. No CTIDs compared across servers: compare stable logical id.

Use versioned generator v1, seed recorded, regular row IDs 1..rows with deterministic
terms including rare/common/medium; append known NULL/empty/unicode/phrase documents
with distinct IDs. Exact generated contents defined once in Python and sent as CSV
COPY to both servers; never independent server random(). Report expected row count
and SHA256 over canonical JSON lines `[id,body]`. Stream fingerprint verification
back from both endpoints before comparing, and again at the end. Frame ID/hash/rows
in a permanent ownership marker for the fixture (provider name, run ID, corpus
version, seed, expected rows/hash). A table matching only a convenient name is not
owned. Reject existing schema on either endpoint before writes. CREATE SCHEMA (not
IF NOT EXISTS), load, build with correct AM, ANALYZE and marker in one transaction
per endpoint. Cross-server preparation is not atomic: if one commits and the other
fails, record partial preparation and leave owned data untouched. No DROP/TRUNCATE,
cleanup, autoinstall, CREATE OR REPLACE or schema adoption is allowed.

## Comparison and evidence

- Default match suite: deterministic term/AND/OR, absent, phrase, NOT, wildcard and
  Unicode cases; queries are data, never user SQL. Optional JSON query-case input
  may contain case name and query only, validated and bounded.
- Compare complete sorted logical ID lists/multisets, not just counts/hashes. Bound
  output with configurable max-results and detect limit+1 as incomplete, never an
  approximate pass. Include mismatch counts and capped examples per case.
- Optional scores: full_score per matching row, id tie-break. Compare finite score
  values with explicit atol/rtol, but separately report float4 wire-bit exactness.
  Compare top-k ordered logical IDs (full_score ORDER BY DESC, id ASC LIMIT k) using
  both providers' native full_score, not Plumb-only top_k. Missing/error is not parity.
- Optional highlights: explicit same-query highlight text, compare by id; restrict
  to bounded matches and report differences. Do not claim ANSI equivalence unless tested.
- Natural EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) samples per endpoint. Record observed
  plan nodes and index references rather than assuming both must use bitmap scans.
  Server planning/execution medians/ranges separate from client round-trip times.
  Different hardware, services, caches, network and PostgreSQL majors are caveats;
  no generic hosted-vs-local performance superiority claim.
- Read-only compare transactions. No shared snapshot exists across independent
  servers; corpus is intended immutable. Fingerprints before/after detect changes
  but cannot prove absence of intervening ABA mutations. Never compare arbitrary
  application tables by default.
- Always write a JSON report and concise Markdown summary in a NEW output directory,
  plus machine-readable statuses (passed/mismatch/incomplete). Nonzero exit for any
  selected parity mismatch, missing capability, error, drift or result cap.
- Do not publish reports or connect external endpoints automatically. Document all
  hosted tests as unverified until actual authorized execution.

## Pure API for independent tests

Runner should expose these helpers without running CLI on import:

```python
validate_run_id(value: str) -> str
quote_ident(value: str) -> str
quote_literal(value: str) -> str
corpus_rows(rows: int, seed: int) -> iterable[tuple[int, str | None]]
corpus_fingerprint(rows) -> str
compare_ids(left: list[int], right: list[int], limit: int = 20) -> dict
compare_scores(left: dict[int,float], right: dict[int,float], atol: float, rtol: float) -> dict
```

`compare_ids` returns at least equal:boolean; multiplicity matters. `compare_scores`
returns at least equal:boolean, missing/extra IDs and bounded mismatches, rejecting
NaN/infinite values. Pure helpers must validate ranges/errors, avoid dict collapse
for duplicate ID comparison, and be deterministic. Unit tests additionally mock
psql failures so missing/unreachable endpoints do not cause writes or fallback.
