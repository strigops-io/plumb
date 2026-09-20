# Implementation status and evidence

## Scope of this increment

Checkpoint 003 integrates the scalar postings core into an **opt-in persistent SQL
index**. A real SELECT uses grouped CTID candidates read from PostgreSQL-managed
WAL-logged pages. It is still a bounded proof of concept; none of the complete
production phases in [VISION.md](../VISION.md) is declared finished. See
[the demonstration and limitations](checkpoint-003-results.md).

Repository: `strigops-io/plumb`. Starting revision:
`bd95c7e51b6afce81396790852ee2f2c169570ad` (Lead-derived baseline).

| Area | Current state | Evidence / boundary |
| --- | --- | --- |
| TINQL, tokenization, scoring, highlighting | Language/algorithms inherited; SQL identity and binding isolated | Language crates unchanged; PostgreSQL behavior assertions preserved with name substitutions |
| SQL extension/API | plumb 0.1.0, plumb schema/AM, ~~> operator | Native identity implemented; optional tin adapter deferred; see coexistence.md |
| Index scans/build/insert/VACUUM | Default heap baseline plus opt-in postings_v1 | Bulk term segment, exact positive candidates, per-insert immutable appends; VACUUM does not reclaim postings |
| CTID identity and validation | Implemented in standalone library | postings/src/lib.rs; no logical-document-ID mapping |
| 256-page groups, masks, sparse offsets | Implemented and persisted in per-term codec frames | Query decodes CTIDs, unions terms across segments and intersects/unions grouped sets |
| Boolean query compilation | Positive terms, AND/OR/conjunction/boost | Unsupported query shapes fall back to heap recheck; never unsafe NOT subtraction |
| Terms, positions, frequencies and stats in storage | Sorted term dictionary implemented | No positions/frequencies/index-backed BM25; metadata stats only |
| Standalone postings serialization | Experimental v1 codec implemented | Canonical little-endian frame, CRC32C, allocation-free validation and explicit limits; docs/postings-format.md |
| Metapage and segment directory | Versioned metapage plus immutable backwards-linked segments | MAIN-fork pages with per-page checksums; no merge/reclamation/generation replacement |
| WAL, publication, recovery and replication | Generic WAL for page writes then head publication | Local immediate-stop recovery passed; replication/PITR and publication-fault campaigns unproven |
| Online writes, HOT, VACUUM liveness and CTID reuse | Per-insert append plus mandatory MVCC/operator recheck | Mutation/concurrency/HOT/reuse cases passed; no liveness mask or dead-byte reclamation |
| Resource/interrupt control | Fixed text/pair/query/payload/page caps and loop interrupts | Not work_mem/RSS accounting; no spill; container/allocator/transient overhead remains |
| SQL baseline harness | Implemented and exercised locally | benches/baseline.sql: deterministic corpus, full ID multiset checks, arithmetic oracle, JSON plans |
| Production readiness / hosted escape hatch | Unproven | No performance, operational or complete hosted-compatibility conclusion yet |

## Checkpoint 003 validation

- **73 extension tests passed**, including 42 executed inside PostgreSQL 17 and
  31 host-side tests. Eight new hardening regressions cover incompatible opclasses
  and query datatypes before unsafe Datum conversions.
- **377 pure Rust tests passed**; 32 postings tests/doctests also passed in release.
- Workspace Clippy with PG17 and pg_test, all targets, passed with warnings denied;
  formatting passed. Full PG18 compile checks using shipped bindings passed, but
  no PG18 server was run.
- Release-build SQL demo: 30,002 rows / 4,286 heap blocks; 30 expected rare matches,
  **30 exact heap blocks and zero lossy blocks**, naturally chosen Bitmap Heap Scan.
  Persistent index: one segment, 356,427 payload bytes, 45 relation blocks.
- 48 demonstration checks, 346 mutation-script checks, 16 HOT/partial-reuse checks,
  54 public Lead coexistence checks passed. REINDEX and TRUNCATE succeeded and
  preserved correctness; actual CREATE INDEX CONCURRENTLY was explicitly rejected.
- Two simultaneous writers, anchored repeatable-read snapshot, rollback candidates
  and subsequent inserts passed. Final fixture: 210 committed rows, 14 beacon hits.
- Immediate-stop/restart with fsync, full_page_writes and checksums ON replayed WAL.
  Original data/index were not rebuilt. Demo (48 checks), mutations (28 checks) and
  concurrent committed/aborted outcomes revalidated; index metadata was unchanged.
- Evidence and limits: [checkpoint-003-results.md](checkpoint-003-results.md).
  This is not a production recovery/replication certification or large-scale benchmark.

## Historical checkpoint 001/002 validation

Local validation on 2026-09-19 UTC, x86-64 Linux, Rust 1.96.0, cargo-pgrx 0.19.1,
PostgreSQL 17.10. All databases were disposable/local; no hosted TIN connection or
private source was used. PostgreSQL source now has independent Plumb identities and
exact operator-OID binding. Inherited tests retain their behavior assertions with
name/operator substitutions; five identity tests were added.

- Checkpoint 001: **354 pure Rust tests passed** (345 inherited tests plus 8 new
  postings tests and 1 new doctest).
- Checkpoint 002 adds the standalone v1 codec: the combined pure Rust suite passes
  **377 tests**, including **32 postings tests/doctests**. The same postings suite
  also passes in release mode. Clippy passes with warnings denied.
- Codec implementation and reference tests were written by separate agents, then
  independently reviewed. Tests pin golden bytes, resealed malformed frames,
  deterministic random inputs, all truncations/single-bit mutations of fixtures,
  inclusive limits and exact allocation failures. A dedicated test binary verifies
  that rejected inputs allocate nothing and partial allocations are released.
- Checkpoint 002 does not change postgres/ or its dependencies. The PostgreSQL and
  coexistence results below are carried forward from the unchanged checkpoint-001
  extension; they were not rerun for a standalone codec-only milestone.
- No 32-bit runtime test or exhaustive fuzzing is claimed; 32-bit CI and longer
  fuzz campaigns remain follow-up validation.
- Postings tests also passed in release mode; `cargo fmt` and Clippy passed for
  the new crate with warnings denied.
- Before the SQL rename, unchanged upstream `cargo pgrx test pg17` passed 36 tests.
  The checkpoint-001 native Plumb suite passed **41 tests**, including 19 in PostgreSQL.
- The SQL harness passed at **1,000 and 100,000 rows** before the rename, with two
  iterations each: 16 plan samples and 4 correctness records per run.
- After updating the harness to `USING plumb` and `~~>`, a native Plumb **1,000-row**
  run with two iterations passed again. The 100k native-Plumb run remains to be
  repeated; the historical 100k result below is explicitly for the Lead identity.
- Public Lead/native Plumb coexistence passed 54 catalog, routing, mixed-helper,
  shadowing and lifecycle checks. See tests/coexistence.sql and docs/coexistence.md.
- Every requested sequential sample used `Seq Scan`; every requested bitmap
  sample used `Bitmap Heap Scan` with the selected provider's index. Complete result multisets
  matched each other and the independent arithmetic oracle.

| Query case | Matches at 1,000 rows (both identities) | Historical matches at 100,000 rows (Lead identity) |
| --- | ---: | ---: |
| rare | 1 | 100 |
| common | 500 | 50,000 |
| rare AND common | 0 | 50 |
| rare OR medium | 10 | 1,090 |

These were **validation runs, not controlled performance benchmarks**: the extension
was a debug build and a test retry briefly overlapped the larger run. The historical runs
reported zero-byte tin indexes (the native 1k run likewise has a zero-byte plumb index), as expected for Lead's no-postings implementation.
At checkpoints 001/002 the scalar library was unintegrated, so those runs showed
no postings SQL improvement. Checkpoint 003 has a separate measured demo above.
The 1m/10m examples have not been run. PostgreSQL 18 and ARM are CI targets, not locally
validated results for this increment.

One initial PostgreSQL test invocation failed because the sandbox lacked `USER`;
setting it to the normal OS account name and rerunning the unchanged suite passed.
This was an environment setup failure, not an extension-code fix.

Reproduce from the repository root after initializing the matching pgrx toolchain:

```sh
cargo test --locked -p plumb-postings -p tinql -p tokenizer -p boldi-vigna
cargo test --locked --release -p plumb-postings
cargo fmt --package plumb-postings --check
cargo clippy --locked -p plumb-postings --all-targets -- -D warnings
cargo pgrx test pg17 --package plumb --no-default-features --features pg17
# In a separate disposable database with this checkout's plumb installed:
psql -X -d plumb_bench -v rows=1000 -v iterations=2 -f benches/baseline.sql
psql -X -d plumb_bench -v rows=100000 -v iterations=2 -f benches/baseline.sql
```

The existing PostgreSQL 17/18 x86-64/ARM CI matrix is retained. A separate pure-Rust
job now exercises the inherited language suites and scalar postings on both
architectures; pull requests also trigger CI. Do not treat job configuration as
proof of a green remote run. Repository writes were blocked by the GitHub
integration (403); no new remote code commit/CI result is claimed. The SQL baseline is opt-in, not automatically run in CI.

## Compatibility evidence ledger

This initial ledger records provenance, not a claim of full TIN equivalence.

| Claim | Provenance | Evidence |
| --- | --- | --- |
| TINQL and SQL helper behavior | Inherited from public Lead source | Language crates unchanged; PostgreSQL helpers renamed to plumb and binding isolated |
| plumb extension/schema/AM, ~~> operator, public Lead coexistence | Plumb-specific implementation and local observation | Exact-OID tests and tests/coexistence.sql; hosted TIN not tested |
| Default heap baseline returns all heap pages | Inherited source behavior | Opt-in postings_v1 instead emits exact positive candidates; see recorded demo |
| Term/AND/OR results on the generated corpus | Independent local observation | baseline.sql checks complete ID multisets against arithmetic predicates |
| Grouped CTID set operations are exact finite-set operations | Plumb-specific implementation | BTreeSet reference tests, exhaustive subset pairs, generated/algebraic tests |
| Hosted TIN behavior beyond inherited surface | Not observed in this increment | No authorized TIN instance supplied/queried |
| Public TIN architecture | Design inspiration, not implementation evidence | Source-material links retained in VISION.md; no claim of identical private internals |

## Next implementation gates

1. Extend the experimental metapage/immutable-segment design with bounded mutable
   ingestion, selective dictionary addressing, generation replacement, merging and
   safe reclamation. No immutable pages are reclaimed in this checkpoint.
2. Broaden WAL publication fault-injection, sustained concurrent mutation and
   all heap-rewrite testing. Verify resource failures leave no missing results;
   existing local tests are necessary but not sufficient release evidence.
3. Compare indexed term/Boolean results with the heap evaluator under concurrent
   snapshots and mutation workloads. Keep operator rechecks for unsupported
   positional/expansion semantics. Define the document universe before NOT.
4. Add bounded ingestion, VACUUM-driven liveness, publication/reclamation and
   recovery tests before describing an index as operationally usable. WAL safety
   is required when persistent state is introduced, not postponed to marketing.
5. Once real SQL pruning exists, run controlled release-build baselines at 100k,
   1m and larger representative datasets, measuring memory, build/write cost,
   latency distributions, index size and maintenance under concurrent load.

## What the vendor-lock-in question still requires

A compatible local syntax layer is only the beginning. A credible hosted-search
escape hatch needs a measured compatibility matrix, representative application
queries (especially ranking/highlighting), data export/import and index rebuild
procedures, backup/restore, upgrades, crash recovery, replication, maintenance and
an operational ownership model. Hosted platform features outside TIN search need
separate assessment. No engineering-time estimate or migration guarantee is
justified by this increment.

Preserve upstream notices and AGPL-3.0-or-later licensing throughout. No proprietary
TIN source, extracted binaries, secrets or confidential documentation form part
of this investigation.
