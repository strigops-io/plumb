# Plumb ↔ TIN: two-instance parity and benchmark runner

`parity.py` connects independently to **one Plumb service and one TIN service**.
It does not require both extensions in one database. Plumb uses `plumb`, `~~>` and
`plumb.*`; the other endpoint uses `tin`, `==>` and `tin.*`.

**Hosted PlanetScale TIN has not been tested by this checkpoint.** A local run against
public Lead validates the harness only. A selected `planetscale-tin` label is an
operator assertion, not automatic proof of provider identity or compatibility.

## Requirements and connection safety

- Python 3.10+ and `psql`/libpq 17 or 18; no Python packages required.
- Run on your own Linux/macOS/WSL environment with network access to both services.
  The CSV loader deliberately accepts safe POSIX temporary-file paths only; native
  Windows preparation is not supported yet. Compare does not load CSV.
- Use **dedicated disposable development/evaluation databases**, never application
  or production tables. The runner must be authorized to create its fixture schema
  on each endpoint. Extensions must already be installed/available: it does not
  create extensions, databases, roles, grants or hosted instances.
- Define two libpq service entries in a private `pg_service.conf`. Do not pass DSNs
  or passwords as CLI arguments and do not paste them into chat or reports.
  Store passwords in a private `.pgpass`/`PGPASSFILE`, or another libpq-supported
  credential mechanism. Follow the provider's approved TLS/CA configuration.

Example service file structure (replace placeholders locally; no password here):

```ini
[plumb_local]
host=127.0.0.1
port=5432
dbname=plumb_eval
user=plumb_eval_user

[tin_eval]
host=YOUR_PROVIDER_GIVEN_POSTGRES_HOST
port=YOUR_PROVIDER_GIVEN_POSTGRES_PORT
dbname=YOUR_EVALUATION_DATABASE
user=YOUR_EVALUATION_USER
sslmode=verify-full
# sslrootcert=/absolute/path/to/provider-approved-ca.pem
```

For local Unix sockets, `host=/absolute/path/to/socket` is also supported. Do not
copy the local plaintext transport assumption to a remote service. Keep remote
TLS enabled and use the hostname/certificate settings supplied by the provider.

```sh
export PGSERVICEFILE=/private/path/pg_service.conf
export PGPASSFILE=/private/path/pgpass
chmod 600 "$PGSERVICEFILE" "$PGPASSFILE"
# Parent directory for NEW result directories:
mkdir -p parity-results
```

The subprocess receives only a simple `PGSERVICE` name, not a connection string.
Ambient routing overrides such as PGHOST/PGPORT/PGDATABASE/PGUSER/PGOPTIONS are
removed so they cannot silently redirect the chosen service. Service-file,
passfile and TLS configuration are retained. `psql -X -w` avoids startup scripts
and interactive password prompts. Raw connection errors/credentials are not
copied into reports. Even invalid CLI arguments use fixed sanitized errors.

## 1. Explicit preparation on both instances

```sh
python3 benches/parity.py prepare \
  --plumb-service plumb_local --tin-service tin_eval \
  --run-id trial01 --rows 1000 --seed 1729 \
  --out-dir parity-results/trial01-prepare \
  --allow-fixture-writes --tin-label planetscale-tin
```

Preparation requires both the `prepare` action and `--allow-fixture-writes`.
Both endpoints must pass read-only capability/identity/schema preflight before
any fixture write. Identical services or identical observed endpoint identities
are refused. This is an accident guard, not proof against proxies or aliases.

Creates only a **new** `plumb_parity_trial01` schema on each endpoint:

- Permanent `docs(id bigint PRIMARY KEY, body text)`.
- The same client-generated deterministic UTF-8 corpus, loaded with local-file
  `\copy` (protocol EOF; no legacy inline `\.` CSV terminator).
- A native `USING plumb(body)` or `USING tin(body)` index; no forced shared reloptions.
- ANALYZE plus a singleton fixture marker recording provider/run/corpus version,
  seed, row count and canonical SHA256 fingerprint.

Default corpus: 1,000 regular rows plus 8 NULL/empty/Unicode/phrase/escaping sentinels.
NULL, empty strings and literal `\N` are distinct. The marker/hash is generated
once by the same Python algorithm, not independent server random functions.

Each endpoint's schema/load/index/marker is transactional. **The two commits are
not distributed-atomic.** If the first commits and the second fails, the report
marks partial preparation and leaves the first intact. A connection lost near
COMMIT has an explicitly unknown outcome. Never assume rollback from a transport
error. Inspect the owned schema and use a new run ID; no automatic cleanup occurs.

Existing schemas are always refused—no DROP, TRUNCATE, CREATE OR REPLACE, reuse or
adoption. The runner never creates application tables outside its fixed prefix.
Temporary CSVs are private and deleted in a finally block, including error paths.

## 2. Compare without fixture writes

```sh
python3 benches/parity.py compare \
  --plumb-service plumb_local --tin-service tin_eval \
  --run-id trial01 --out-dir parity-results/trial01-compare \
  --samples 3 --with-scores --with-highlights \
  --top-k 20 --atol 0.000001 --rtol 0.00001 \
  --tin-label planetscale-tin
```

`compare` is also the default action. All its database operations are explicitly
read-only. It validates ownership markers, permanent table shape, primary key,
exactly one valid native body index and full corpus fingerprints before comparing.
Index definitions/options/opclass names are recorded per endpoint. Fresh final
transactions repeat the corpus and index-configuration checks; drift is incomplete,
not a PASS. There is no shared cross-server snapshot, and before/after hashes cannot
rule out an intervening change-and-change-back (ABA) sequence. Keep fixtures immutable.

`--tin-label` is a fixed category: `unverified` (default), `public-lead`, or
`planetscale-tin`. It intentionally does not accept arbitrary labels or connection
strings. None of the categories independently verifies the endpoint's provenance.

### Selected suites

| Suite | Comparison |
| --- | --- |
| Match IDs (always) | Complete sorted logical IDs/multisets, not just counts or hashes |
| `--with-scores` | Per-ID native full_score with explicit tolerances; float4 wire-bit equality reported separately |
| Ranking with scores | Native full_score ORDER BY score DESC, **id ASC**, LIMIT top-k on each provider |
| `--with-highlights` | Explicit same-query HTML highlight text by logical ID |

**Never compare CTIDs across servers**; physical storage layouts differ. Ranked
queries use each provider's native full_score, not Plumb's separate top_k API.
A score passes tolerance when `abs(plumb-tin) <= atol + rtol*abs(tin)`; ranking is
checked separately and exact ID-order differences still count as mismatches.
NaN/infinity, invalid score bits, duplicate IDs or changing ID sets are incomplete.
ANSI highlighting is not covered.

Default cases: common/medium/rare terms, AND, OR, absent, phrase, NOT, wildcard and
Unicode. Custom cases are query data only:

```json
[
  {"name": "phrase_example", "query": "\"quick brown\""},
  {"name": "boolean_example", "query": "common AND medium"}
]
```

Pass `--cases-json path/to/cases.json`. It is not an arbitrary SQL workload file.
Queries are safely quoted; no identifiers/operators/services come from query text.
A missing/failed optional capability is **incomplete**, never silently skipped.
A selected suite must actually execute nonempty results on both endpoints; two
empty result sets cannot masquerade as verified scoring/highlighting support.

### Reports and exit codes

Every valid run writes `report.json` and `summary.md` into a **new**, private output
directory under an existing parent. Existing reports are never overwritten.

- Exit **0**: selected comparison suites passed, or explicitly labelled preparation passed.
- Exit **1**: mismatch/incomplete database run (read the report).
- Exit **2**: invalid CLI/output setup or report-writing failure.

Reports retain mismatch counts and capped examples, capabilities, fixture hashes,
index definitions, query outcomes and natural EXPLAIN JSON samples. They identify
endpoint roles/versions, not secret connection strings. Raw server stderr is not
archived; errors have safe classifications. Diagnose provider permission/TLS issues
locally using the configured service, without publishing credentials or raw logs.

## Performance interpretation and bounds

Natural `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` is captured independently on both
servers. The runner records actual plan nodes/index references; it does not insist
that TIN share Plumb's bitmap plan. Server planning/execution medians and ranges are
separate from client round-trip timing, which includes connection/network/psql.
Different hardware, PostgreSQL majors, caches, configurations and build profiles
make a local-versus-hosted timing ratio unsuitable as a generic superiority claim.
Record provider tier, settings and installed revision/build separately.

Start with **1,000 rows**. Supported regular row range is 1–200,000; fixture includes
8 additional sentinels. Default `--max-results` is 20,000 (max 200,008): limit+1
checks make a capped result incomplete, never a truncated PASS. For larger runs
raise this deliberately. Scoring every matching row can be costly, especially
with heap-backed implementations; enable optional suites only after a small run.
`--samples` is 1–30, `--timeout` 1–3600 seconds (default 120), `--top-k` 1–10,000.
Each SQL call has a timeout and the client has a slightly longer process deadline.
Fixed caps bound client data sizes but are not a database memory or runtime guarantee.

## Validation in checkpoint 006

- **55 offline unit tests** passed, including quoting, corpus distinctions, result
  multiplicities, tolerances, credential-error suppression, no-write preflight,
  partial commit, caps, drift and CSV cleanup.
- Live **two-server** test: Plumb release on PostgreSQL 18.6 versus unchanged public
  Lead (`tin` 1.0.3) on PostgreSQL 17.10, using separate local libpq services.
- 1,008 rows per server; identical canonical SHA256 before/after. All 10 cases passed
  IDs, score tolerances, float4 wire bits, stable-ID ranking and HTML highlighting.
- Live result-cap and existing-schema negative cases returned incomplete/nonzero.
- First integration attempts caught removed lc_ctype/lc_collate settings and PG18
  CSV terminator behavior; catalog locale reads and local-file COPY fixed them.
- This is **not hosted PlanetScale TIN evidence**. The public Lead build and Plumb
  profiles were not normalized for performance comparison; retained timings are
  harness evidence only.

Run offline tests with:

```sh
python3 -W error -B -m unittest discover -s benches -p test_parity.py -v
```

The original `baseline.sql` remains an explicit-heap single-instance baseline;
this runner uses the default Plumb engine on a permanent shared corpus instead.
