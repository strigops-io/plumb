# Implementation status and evidence

## Scope of this increment

This is Phase 0 groundwork and the **scalar postings portion** of Phase 1 in
[VISION.md](../VISION.md). Neither phase is declared complete. The first milestone
keeps the postings algorithm separable from PostgreSQL while establishing an
independent Plumb SQL identity, coexistence contract and repeatable baseline before
introducing persistent index state.

Repository: `strigops-io/plumb`. Starting revision:
`bd95c7e51b6afce81396790852ee2f2c169570ad` (Lead-derived baseline).

| Area | Current state | Evidence / boundary |
| --- | --- | --- |
| TINQL, tokenization, scoring, highlighting | Language/algorithms inherited; SQL identity and binding isolated | Language crates unchanged; PostgreSQL behavior assertions preserved with name substitutions |
| SQL extension/API | plumb 0.1.0, plumb schema/AM, ~~> operator | Native identity implemented; optional tin adapter deferred; see coexistence.md |
| Index scans/build/insert/VACUUM | Original heap-backed Lead path | postgres/src/am.rs; no persistent search entries; every heap page remains a candidate |
| CTID identity and validation | Implemented in standalone library | postings/src/lib.rs; no logical-document-ID mapping |
| 256-page groups, masks, sparse offsets | Implemented in memory | Scalar union, intersection and finite set difference; canonical immutable representation |
| Boolean query compilation | Not implemented for postings | Existing TINQL heap evaluator remains authoritative; library difference is not SQL NOT |
| Terms, positions, frequencies and stats in storage | Not implemented | No dictionary, position stream or index-backed BM25 |
| Standalone postings serialization | Experimental v1 codec implemented | Canonical little-endian frame, CRC32C, allocation-free validation and explicit limits; docs/postings-format.md |
| Metapage and segment directory | Not implemented | Codec is not a PostgreSQL storage format or production migration path |
| WAL, generation publication, recovery and replication | Not implemented for postings | The new library is not used by the access method |
| Online writes, HOT, VACUUM liveness and CTID reuse | Not implemented for postings | Heap-backed baseline behavior must not be confused with persistent-index correctness |
| Bounded PostgreSQL memory/interrupts | Not implemented in postings | Rust vectors allocate whole inputs/results; not charged to PostgreSQL contexts |
| SQL baseline harness | Implemented and exercised locally | benches/baseline.sql: deterministic corpus, full ID multiset checks, arithmetic oracle, JSON plans |
| Production readiness / hosted escape hatch | Unproven | No performance, operational or complete hosted-compatibility conclusion yet |

## Validation performed

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
  The native Plumb suite now passes **41 tests**, including 19 in PostgreSQL.
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
No SQL speedup can be attributed to the unintegrated scalar library. The 1m/10m
examples have not been run. PostgreSQL 18 and ARM are CI targets, not locally
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
| Existing access method returns all heap pages | Public source inspection | postgres/src/am.rs, especially amgetbitmap; local plans and zero-byte index sizes |
| Term/AND/OR results on the generated corpus | Independent local observation | baseline.sql checks complete ID multisets against arithmetic predicates |
| Grouped CTID set operations are exact finite-set operations | Plumb-specific implementation | BTreeSet reference tests, exhaustive subset pairs, generated/algebraic tests |
| Hosted TIN behavior beyond inherited surface | Not observed in this increment | No authorized TIN instance supplied/queried |
| Public TIN architecture | Design inspiration, not implementation evidence | Source-material links retained in VISION.md; no claim of identical private internals |

## Next implementation gates

1. Design a PostgreSQL metapage/segment directory and page/extent layout around the
   standalone postings codec. Keep extension version and format version distinct.
   The codec is only one component: ownership, locking, publication and recovery
   contracts remain unresolved before wiring buffers into scans.
2. Implement PostgreSQL-managed, WAL-safe bulk storage and exact term candidate
   CTIDs. A build-only milestone must explicitly reject unsupported mutations or
   maintain a provably correct fallback—silently stale postings are unacceptable.
   Include HOT-root handling, partial/expression indexes, NULL/empty values and
   rewrite semantics in the design.
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
