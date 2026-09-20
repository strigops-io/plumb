# Checkpoint 005: query optimization contract

This checkpoint targets measured bottlenecks without changing CTID/MVCC semantics.
Real PG17 and PG18 runtime tests are required. No optimized path may silently change
results, accept corrupt selected data or rely on unsupported CPU instructions.

## Shared acceleration API (plumb-postings)

Expose public `accel` module, with scalar reference implementations retained:

```rust,ignore
pub struct Crc32c { /* incremental */ }
impl Crc32c {
    pub fn new() -> Self;
    pub fn update(&mut self, bytes: &[u8]);
    pub fn finish(self) -> u32;
}
pub fn crc32c(bytes: &[u8]) -> u32;
pub fn crc32c_scalar(bytes: &[u8]) -> u32;
pub fn crc32_ieee_parts<'a>(parts: impl IntoIterator<Item=&'a [u8]>) -> u32;
pub fn mask_and(a: [u64;4], b: [u64;4]) -> [u64;4];
pub fn mask_or(a: [u64;4], b: [u64;4]) -> [u64;4];
pub fn crc32c_backend() -> &'static str;
pub fn mask_backend() -> &'static str;
```

CRC32C uses runtime SSE4.2 dispatch on supported x86-64 and safe table-based scalar
fallback elsewhere; IEEE CRC32 remains the correct distinct polynomial, with a
fast scalar table implementation. Page masks may use AVX2 only after runtime feature
checks. All unsafe code must be confined to a documented backend module; keep safe
public interfaces and deny unsafe everywhere else. Normative bytes and checksums
stay unchanged, including golden v1 frames. Preserve interrupt points in the PG
callers by updating CRC in bounded chunks. Add deterministic scalar/accelerated
parity tests and portable fallbacks; unsupported CPUs must work.

## Selective term reads and advisory statistics

New immutable term payloads may use an explicit **version 2 term directory** while
retaining existing PostgreSQL page envelopes/metapages and scalar posting-frame v1.
The term directory must have its own checksum covering every term, offset, length
and stored posting count; fixed headers independently validated. Posting frames
retain their own checksums and PG page envelopes remain validated for every page
read. New code reads legacy v1 segments via the existing full-validation path;
merge accepts both and writes the new format. Unknown versions fail closed.

Provide a bounded storage range-reader over a captured immutable segment descriptor.
Validate descriptor/root/ordinal/chunk CRC before copying selected ranges; all range
arithmetic and allocation requests are bounded. Keep full traversal for merges and
full validation. Ordinary selective scans validate directory and selected frames,
not unread frame bytes; say so rather than claiming a full-index corruption scrub.

Queries load/check directories then fetch only needed term-frame ranges. Absent
terms should avoid scanning all posting payload pages. Multiple terms must share
one per-segment directory read. Union per-term CTIDs across segments before Boolean
combination, preserving rechecks and historical CTIDs. Never use stale posting
counts to filter rows or as exact snapshot-visible document frequency.

Expose `plumb.term_stats(index regclass, query text)` as an owner-authorized advisory
inspection API, showing physical posting count upper bounds, segments/version and
estimated cardinality. Refine AM cost estimates for supported constant queries
using bounded lookup; fallback for parameters/unsupported expressions and preserve
plan-time safety. Index posting counts include dead/aborted/duplicate histories.
Heap selectivity without an operator restriction estimator may remain coarse; do
not describe index-level estimates as complete planner statistics.

## Exact bounded top-k, explicitly not WAND

Add `plumb.top_k(index regclass, query text, k int DEFAULT 10)` returning `(ctid tid,
score real)`, descending full-BM25 score with deterministic ascending-CTID ties.
Equivalent reference: the same indexed expression/partial predicate and visible
corpus as `plumb.full_score`, applying the matching query and ordering by score DESC,
ctid ASC LIMIT k. Preserve floating-point term order and inherited TF bucketing.

This first API may stream the visible heap corpus once to compute BM25 statistics
and bounded candidate features, then retain only k final results with a bounded
heap rather than sorting/materializing scores for every document. State clearly
that corpus scanning remains, not index-backed BM25 or block-max/WAND pruning.
Honor SELECT privileges, column permissions and RLS using caller-context SQL/SPI;
never raw-heap bypass. Use one statement snapshot for statistics and candidates.
Reject unsupported indexes/states or resource budgets explicitly. No public arbitrary
expression execution or SECURITY DEFINER escalation. Identifiers from catalogs must
be quoted and query text passed as data. Expressions/partial predicates must either
work with exact semantics or be explicitly rejected, never silently ignored.

Bound k (0..1000), query shape/terms, scanned corpus and candidate memory. Return
errors before unbounded allocation. Fix inherited scoring cache if statement/snapshot
identity can otherwise allow stale reference scores across statements. Do not change
the existing SQL scoring signatures or claimed semantics.

## Gates

- Golden bytes, accelerated/scalar random/boundary parity and all corruption tests.
- Legacy v1 and new v2 lookup/merge correctness, selective absent/rare buffer counts,
  term metadata corruption, bad ranges and unknown-version rejection.
- Exact top-k vs full-score references with ties, boosts, NULL/empty, MVCC/rollback,
  expressions/partial predicates, role permissions and RLS; resource/argument caps.
- PostgreSQL 17 and 18 actual server tests, not merely shipped-binding compilation.
- Release benchmark before/after on the same controlled corpus/query settings;
  report medians/ranges and cold/warm caveats, not only best samples.
- Existing default/build/merge/coexistence/recovery invariants remain intact; no
  production, hosted-TIN, replication/PITR or hardware-portability overclaim.
