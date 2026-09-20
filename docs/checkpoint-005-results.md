# Checkpoint 005: reviewed optimizations, PG18 runtime and archive audit

**Proof of concept only.** This checkpoint commits the formerly unfinished query
optimization work after review, real PG17/18 execution and measured release runs.
It also explains the apparent uncommitted files reported after ZIP extraction.

## Archive finding: modes, not missing commits

The four original full-history ZIPs (001–004) were each extracted twice and audited
against `git show HEAD:path`. **Every tracked file was byte-identical to its commit**,
all manifests/history counts matched, and all Git integrity checks passed.

Permission-dropping extraction alone changed `100755` to `100644` for:

- script/materialize-private-regress
- script/run-private-regress
- tests/merge_concurrency.py (checkpoint 004 only)

Restoring the ZIP's Unix mode metadata made all four trees clean. No prior archive
was modified. This reproduces a likely cause; if your changed filenames or content
diffs differ, inspect those separately rather than discarding them.

The new [permission helper](../script/check-checkpoint-permissions.py) compares file
bytes to HEAD and changes only executable flags of byte-identical regular files.
It skips modified/missing/symlink files and never stages/resets files or changes Git
config. Its regression test proves user-modified contents remain untouched. See
[checkpoints.md](checkpoints.md); plain extraction can still drop modes on some
platforms, so a helper is not a promise that every ZIP GUI preserves them.

## Implemented query work

### Selective term reads

New immutable segments use **term-payload version 2**, with a separately checksummed
sorted directory of term names, frame ranges and physical posting counts. Queries
load one directory per segment and fetch only requested posting frames. An absent
term needs no frame requests. The first page may contain both directory and frame
bytes, so directory-only logical access is not zero physical I/O.

The metapage/page format and `storage='postings_v1'` remain version 1; these are
separate from the term payload. Scalar CTID frames also remain v1. Existing term-v1
segments take a fully validated legacy path; new inserts/builds and real merges
write v2. Mixed v1/v2 merging is supported. A zero/one-segment merge is still a no-op,
not a forced version converter.

Range reads check captured immutable descriptor identity, bounds, ordinal, chunk
length and page-envelope CRC before copying. The directory checksum covers names,
offsets, lengths and counts; selected frames retain their own checksums. Physical
PostgreSQL offset validation occurs immediately after selected frame decode, before
Boolean operations can eliminate invalid candidates. Full merge validates all data.

**Corruption boundary:** selective scans do not verify unread frames/pages. Advisory
planner reads fall back on application-level Corruption/Budget errors; ordinary
execution and owner-only inspection remain fail-closed. PostgreSQL-origin page I/O
or checksum errors still propagate—cancellation, OOM, permissions and arbitrary
errors are not swallowed.

### Statistics and costing

```sql
SELECT plumb.term_stats('documents_plumb_idx'::regclass, 'beacon AND copper');
```

The owner-authorized API reports term-payload versions, physical count upper bounds,
a bounded cardinality heuristic and fetched payload bytes. Counts include stale,
aborted and duplicate histories; `exact_visible_df` is explicitly false.

AM cost estimation now uses supported constant queries and bounded directory reads
(up to 32 segments / 256 KiB fetched payload); parameters, oversized inputs and
unsupported shapes retain conservative costing. There is no global operator
restriction estimator. In the growth sample, the **Bitmap Index Scan** estimate
improved from 20,000 to 200 for 200 actual rows; the heap node still estimated 100,000.
This is useful partial progress, not complete planner statistics.

### Hardware acceleration, with measured selection

CRC32C uses SSE4.2 after runtime detection; unsupported CPUs use portable tables.
IEEE page CRC32 uses its distinct polynomial and a fast portable implementation.
Checksums/golden bytes are unchanged. Unsafe intrinsics are confined to one private
backend; public interfaces validate lengths/dispatch and remain safe.

An AVX2 four-word page-mask implementation is included and parity-tested as an
explicit experimental path. It was **1.8–1.9x slower** than scalar in the measured
microbenchmark, so default masks remain scalar. CRC32C dispatch measured roughly
8.7 GiB/s versus 1.7 GiB/s for portable tables on this host; these are microbenchmarks,
not whole-query speedups. AArch64 compilation passed; no ARM runtime claim.

### Exact bounded top-k

```sql
SELECT * FROM plumb.top_k('documents_plumb_idx'::regclass, 'beacon', 10);
```

Returns `(ctid tid, score real)`, ordered full-BM25 score descending and CTID ascending
for ties. Shared scoring arithmetic, TF bucketing and term reduction order are
preserved. A bounded heap retains only k final results; compact matching features
are held until corpus statistics are known.

**It still scans the caller-visible heap corpus.** This is not index-backed BM25,
WAND, block-max pruning or persisted frequency/length statistics. The first API
accepts plain text-column indexes and bounded positive term/AND/OR/boost queries;
partial/expression indexes, inheritance parents and advanced shapes are explicitly
rejected rather than silently changing semantics.

Budgets: k 0–1000; 4096 query bytes; 64 normalized scoring-term occurrences;
256 nodes / 32 depth; 1,000,000 visible rows; 256 KiB per document; 256 MiB corpus
text; 32,768 tokens/document; 100,000 matching candidates; 32 MiB charged candidate
features. These are not a complete RSS/SQL-policy-work guarantee.

## Security and snapshot corrections found during review

- Full-score corpus loading now explicitly uses the caller's statement snapshot,
  matching top_k; a preceding mutable SPI metadata call no longer silently selects
  a newer READ COMMITTED snapshot.
- Cache identity includes statement/transaction/snapshot/user information. RLS
  relations do not reuse corpus caches; policies can depend on session state even
  within a statement. This can make RLS full_score expensive; top_k remains bounded.
- The inherited helper EXECUTE revocation prevented ordinary callers from using
  rewritten full_score. Its invoker helper now has public execution **with** index/
  heap/AM/state validation and normal SELECT/RLS enforcement on misses and hits.
  No SECURITY DEFINER, raw-heap bypass or role switch was introduced. Direct helper
  denial, wrong-column access, forged relation pairs, invalid modes and RLS policy
  changes have negative regressions.
- Internal operators are catalog-qualified. Prepared corpus plans are checked
  against the locked heap OID, and the exact checked plan is executed, preventing
  namespace rename/replacement from redirecting name-based internal queries.
- PG18 EXPLAIN emits floating numeric row fields; tests now compare exact numeric
  values, not JSON integer representation. Expected rows/checks were not removed.

## Actual validation

- **170 tests passed on PostgreSQL 17.10 and 170 on PostgreSQL 18.6**, including
  109 PostgreSQL tests and 61 host-side tests per run. PG18 was built/run as a real
  parallel installation; this is no longer shipped-binding compilation alone.
- **383 pure Rust tests passed**, plus **38 release postings tests/doctests**;
  workspace Clippy (warnings denied), formatting and AArch64 postings compilation
  passed. Scalar/accelerated parity, golden bytes, corruption, allocation failures,
  v1/v2 ranges, merging, permissions, RLS and score-bit ordering are exercised.
- Release runs preserved the same heap fixtures: v1 was queried with new code before
  explicitly REINDEXing the same three indexes to v2. All 12 fixture/query cases
  matched full sequential IDs and checkpoint-004 ID references.
- Top-k matched **ordered CTIDs and exact float4 wire bits**, not approximate scores.
- Separate persistent READ COMMITTED and repeatable-read sessions passed 32 scoring
  statements and 96 null-XID checks with concurrent insert/update/delete commits.
  Anchor-document scores changed with corpus statistics; old RR retained old results.
- PG17 v2 survived a further immediate stop/restart; IDs, top-k bits and reference
  results passed without rebuilding. This is not replication/PITR certification.
- The original full run exposed shared ranking failures and PG18 representation
  failures; those were investigated and fixed before the final green matrix. One
  initial test invocation was terminated by the tool's 120-second command window;
  final matrix drivers ran to completion with status files and no timeout claim.

## Release measurements (three samples per route)

Same 200k-row tiny-vocabulary growth fixture, same settings, fresh backend per
operation, caches warmed by correctness/diagnostic reads, no OS-cache flush.
Execution excludes planning; full plans and min/max/sample arrays are retained.

| Query | Checkpoint-004 median execution | New code, old v1 | New code, rebuilt v2 |
| --- | ---: | ---: | ---: |
| beacon | 18.192 ms | 7.459 ms | 2.071 ms |
| absentmarker | 16.073 ms | 5.538 ms | 0.190 ms |
| beacon AND copper | 20.887 ms | 8.735 ms | 3.398 ms |
| beacon OR markerone | 19.767 ms | 8.606 ms | 3.258 ms |

The v2 rare Bitmap Index Scan had **41 shared buffer hits**, absent had **26**,
versus 272 baseline hits even for absence. These are buffer accesses, not unique
pages or physical disk reads. Advisory directory inspection fetched 129,920 payload
bytes versus 2,032,885 in v1. V2 planning medians were about 0.95–1.04 ms; heap-level
row selectivity remains coarse.

On the 30,002-row demo corpus, updated full_score+sort LIMIT 10 took **948.847 ms**
median and top_k took **494.644 ms**, with exact output parity. The older 1873.130 ms
reference used a slightly different projection/tie order, so it is contextual—not
a bit-exact same-SQL speedup claim.

These are **small synthetic warm measurements**, not controlled fleet results,
large-vocabulary/million-row scalability or hosted TIN comparisons. The code changes
include several optimizations together; no independent CPU attribution is claimed.

## Remaining boundaries

No positions/frequencies persisted, WAND, top-k index executor, global selectivity
estimator, mutable ingestion buffer, subset merge, dead-byte reclamation or physical
cap/fault campaign. Full_score keeps its inherited all-document memory behavior;
public helper hardening does not make that API bounded. Top_k is bounded separately.
Same physical/logical index caps and default-analyzer restrictions remain. No stable
production upgrade path or supported arbitrary old checkpoint downgrade: old code
cannot read new v2 payloads. Fresh databases remain the recommended test workflow.

## Files and evidence

- [Optimization contract](query-optimization-contract.md)
- [Archive audit summary](evidence/checkpoint-005-archive-audit.json)
- [Benchmark summary](evidence/checkpoint-005-summary.json)
- [Old baseline](evidence/checkpoint-005-baseline.json)
- [New code/v1 benchmark](evidence/checkpoint-005-newcode-v1.json)
- [Rebuilt v2 benchmark](evidence/checkpoint-005-rebuilt-v2.json)
- [Recovered v2 verification](evidence/checkpoint-005-recovered-v2.json)
- [Persistent-session scoring evidence](evidence/checkpoint-005-scoring-freshness.json)
- [Reproduction harness](../tests/query_optimization_bench.py)
- [Snapshot reproduction](../tests/scoring_freshness.py)

The benchmark of existing checkpoint-004 data used a controlled disposable-fixture
registration of the new SQL functions, not a claimed general extension upgrade.
The full-history checkpoint ZIP is committed and verified; extraction mode repair
is separate from source-content changes and never discards user edits.
