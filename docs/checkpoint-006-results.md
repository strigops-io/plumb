# Checkpoint 006: independent Plumb/TIN parity runner

This checkpoint changes benchmark tooling, not the Rust extension. It enables two
separate connection targets so local Plumb can be compared with an explicitly
authorized PlanetScale TIN evaluation instance.

**No hosted PlanetScale instance was contacted or verified.** The live test used
unchanged public Lead's `tin` interface as the right-hand local provider. That is
harness validation, not evidence of complete TIN compatibility or hosted performance.

## Delivered

- `benches/parity.py`: Python stdlib + psql, separate Plumb/TIN libpq services.
- Explicit write-consented `prepare`; read-only `compare` is the default.
- Same deterministic client-generated permanent corpus on both endpoints, with
  marker/version/seed/SHA256 verification and native provider index creation.
- Complete logical-ID/multiset comparison; optional full-score tolerances and wire
  bits, native top-k with logical-ID tie-break, and explicit-query HTML highlights.
- Natural EXPLAIN JSON samples, server planning/execution timing and separate client
  round-trip timing; no forced shared plan shape or hosted performance claim.
- JSON and Markdown reports in a new directory, nonzero exit on mismatch/incomplete.
- Credential-safe service/passfile usage, fixed provenance categories, sanitized
  parser/transport errors, schema collision refusal, partial-commit reporting and
  before/after corpus plus index-definition drift checks.
- User guide, sample query-case JSON and a dedicated offline CI job.

See [TWO_INSTANCE_PARITY.md](../benches/TWO_INSTANCE_PARITY.md) for prerequisites,
connection/TLS setup, actual CLI commands, caps and interpretation.

## Validation actually performed

### Offline

**55 unit tests passed**, with warnings treated as errors. Cases cover quoting and
schema/service injection attempts, deterministic NULL/empty/Unicode corpus framing,
multiset multiplicity, score tolerances/nonfinite values, strict score-wire IDs/bits,
CLI credential echo suppression, read-only preflight, both-endpoints-before-writes,
partial/unknown commits, result caps, selected missing capabilities, data/config
 drift and temporary CSV cleanup.

### Two real, separate local servers

| Endpoint role | Actual local implementation | Server |
| --- | --- | --- |
| plumb | Plumb 0.1.0, checkpoint-005 release code | PostgreSQL 18.6 |
| tin | Unchanged public PlanetScale Lead 1.0.3 | PostgreSQL 17.10 |

Separate PostgreSQL data directories, socket directories, ports, databases and
libpq service entries were used. The service file stayed outside the repository
and contained no password. Extensions were installed explicitly by the test
operator, not by the harness. The databases were fresh disposable fixtures.

Preparation created 1,008 documents per endpoint (1,000 regular + 8 sentinels).
Canonical corpus SHA256 was identical before and after comparisons:

`da01637e2975e9d8f2a9e898d4125769e7dc94a94427611b3f73cdd5ce9a9290`

All **10 cases passed**: common, medium, rare, AND, OR, absent, phrase, NOT, wildcard
and Unicode. Every case matched logical IDs, score tolerance, exact float4 wire
bits, native ranking IDs and explicit-query HTML highlights. The first full run
collected three timing samples per endpoint/case; after adding index-definition
recording, a final full-suite comparison with one sample again passed.

Live negative cases:

- `--max-results 5`: result-cap exhaustion produced incomplete/nonzero, never a
  truncated parity pass.
- Re-preparing the existing run ID: both preflights detected schemas and refused
  before fixture writes. No existing data was overwritten or removed.

No extension source changed; the previous checkpoint's 170-test PG17 and PG18
extension suites were not rerun for this benchmark-only change. The new runner
was exercised against both actual server majors as described above.

## Issues caught and fixed during implementation

- Naive offline write detection falsely matched the privilege literal `'CREATE'`
  inside a read-only SELECT. Tests now inspect submitted SQL with strings/comments
  masked while retaining explicit read-only/no-write assertions.
- Default argparse error text could echo a mistakenly supplied credential. Parser
  failures now use fixed messages; provider labels are restricted to three fixed
  categories rather than persisting arbitrary text.
- `lc_ctype`/`lc_collate` were incorrectly read as configuration settings in the
  initial preflight. Locale metadata now comes from pg_database, working on both
  actual server versions. No writes occurred during that failed preflight.
- Inline CSV `\.` termination failed with the PG18 server/client combination.
  A private local CSV and psql `\copy FROM file` now end through protocol EOF,
  preserving NULL, empty strings, literal `\N`, quotes and trailing backslashes.
  The earlier failed preparation was recorded as commit-unknown conservatively;
  a fresh run ID was used after correction, without cleanup/adoption.
- Index definitions/options are now recorded and checked for same-endpoint drift;
  provider-specific definitions are deliberately not required to be textually equal.

## Limits and how to use hosted TIN

Use dedicated evaluation databases and local libpq service/passfiles with the
provider's TLS/CA settings. Never paste credentials into reports or chat. Preparation
needs CREATE-schema/table/index privileges on both endpoints; comparison needs the
fixture/capability reads. Missing hosted capabilities or privileges are an explicit
incomplete result, not permission to install extensions or substitute Lead.

The two preparation commits are not atomic across servers; transport loss near
COMMIT has unknown outcome. The runner leaves owned fixtures for inspection and
never drops/truncates existing objects. Independent servers share no snapshot;
keep the corpus immutable and interpret before/after fingerprints as drift checks,
not proof against change-and-change-back histories.

The local Lead/Plumb build profiles and host conditions were not normalized, so
retained local timings demonstrate report generation only. They are not a measured
comparison with hosted TIN. Corpus coverage is limited, score tolerance is explicit,
ranking is separately exact, and ANSI highlighting is not covered.

## Retained evidence

- [Final local comparison](evidence/checkpoint-006-parity.json)
- [Human-readable local summary](evidence/checkpoint-006-parity.md)
- [Initial three-sample run](evidence/checkpoint-006-three-sample.json)
- [Preparation report](evidence/checkpoint-006-prepare.json)
- [Cap-exhaustion rejection](evidence/checkpoint-006-cap.json)
- [Existing-schema rejection](evidence/checkpoint-006-existing.json)

All connection identities/service names are omitted from runner reports. Query
text, fixed fixture IDs, hashes and index definitions are intentional evidence;
review any report before sharing it externally. Nothing was published to a hosted
service or GitHub by this checkpoint.
