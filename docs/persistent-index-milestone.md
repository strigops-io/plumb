# Persistent postings milestone: checkpoint-003 implementation contract

> Historical checkpoint-003 contract. Checkpoint 004 changes the default to
> postings_v1, adds bounded multi-segment builds and manual immutable merging; see
> [default-engine-and-merge.md](default-engine-and-merge.md).

This milestone now demonstrates actual SQL pruning from stored grouped CTID
postings while retaining a correct fallback on PostgreSQL 17.10. It is an experimental implementation,
not a production readiness claim. This document is the agent integration contract;
validation outcomes are recorded in [checkpoint-003-results.md](checkpoint-003-results.md).

## Explicit opt-in

`CREATE INDEX ... USING plumb(body) WITH (storage = 'postings_v1')` selects the new
path. The default `storage = 'heap'` retains the existing compatibility baseline.
Once an index contains the new metapage, that persisted identity is authoritative
for scans and writes, even if ALTER INDEX changes the storage reloption; switching
formats requires REINDEX. Otherwise toggling reloptions could silently skip writes.
A zero-page relation selected as postings_v1 must represent a freshly reset empty
index, never a previously built postings index missing its metadata. The implementation
must make this distinction safe (initialize from AM build/empty callbacks and fail
closed on unexpected missing metadata).

Only the default text analysis pipeline is supported in postings mode initially.
Validate at build/scan/insert; reject incompatible analyzer reloptions rather than
silently returning wrong results. BM25-only options can remain independent. The
normal text operator uses the same inherited default pipeline for heap rechecks.

## Storage and publication

All bytes live in PostgreSQL's index relation MAIN fork, with standard PostgreSQL
page headers, buffer locks and Generic WAL records. No external files.

Block zero is a versioned metapage. It records magic, format version, head immutable
segment block, segment count and committed logical payload bytes. Each immutable
segment has a versioned first-page descriptor (previous segment root, payload length,
page count) followed by bounded payload chunks. Page headers/descriptors are validated
before trusting lengths or copying bytes. Frame corruption produces an error.

Serialize appenders with the metapage's exclusive buffer lock. Write all new segment
pages with Generic WAL, then publish the new head/counters in a separately WAL-logged
metapage update. Incomplete unreferenced appended pages are never queried; reclamation
is deferred. Scans copy a committed head/counters snapshot under shared lock and
traverse immutable linked segments without holding the metapage lock across the
whole scan. There is no segment deletion/reuse/merge in this increment.

Initial bulk build produces one immutable segment. Subsequent aminsert calls append
small immutable segments with their own CTIDs; this is deliberately not the final
bounded mutable-segment design. Recheck every exact candidate against the heap.
Aborted/deleted/reused CTIDs can remain stale positive candidates, never a visibility
authority. With no posting subtraction for NOT, stale candidates cannot hide a real
positive match. VACUUM does not yet reclaim posting bytes; capacity exhaustion fails
writes with a clear error. No production maintenance claim.

Generic WAL ordering ensures referenced pages precede publication in WAL. Validate
immediate-stop restart behavior locally; a successful test is evidence, not proof of
PITR/replication correctness. Reject CREATE INDEX CONCURRENTLY and non-permanent heap
relations in this milestone unless explicitly implemented and tested. REINDEX and
TRUNCATE must be tested or rejected safely; never silently trust stale storage.

Bounds: at most 16 MiB per segment, 64 MiB committed payload across an index, and
65,536 committed segments. Use interrupt checks in page/segment/build loops and
bound term/posting builder memory explicitly. PostgreSQL work_mem integration is
not yet implemented; document fixed caps and transient allocator overhead honestly.

## Storage module API shared between agents

`postgres/src/storage.rs` owns PostgreSQL page/WAL IO only:

```rust,ignore
pub const MAX_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SEGMENTS: u32 = 65_536;
pub struct StorageStats {
    pub format_version: u32,
    pub segments: u32,
    pub payload_bytes: u64,
    pub relation_blocks: u32,
}
// Relation must be exclusively owned/new and have zero blocks; initialize meta,
// write first immutable payload and publish its head. Empty payload is allowed.
pub unsafe fn build(index: pg_sys::Relation, payload: &[u8]);
// Relation must already have valid metapage; serializes concurrent writers.
pub unsafe fn append(index: pg_sys::Relation, payload: &[u8]);
// Returns None for a zero-block heap-baseline relation; errors on nonempty invalid.
pub unsafe fn stats(index: pg_sys::Relation) -> Option<StorageStats>;
// Bounded snapshot traversal, newest to oldest, copy one immutable payload at a
// time and release its buffer pins before callback. Count/length/chain checks.
pub unsafe fn visit(index: pg_sys::Relation, visitor: impl FnMut(&[u8]));
```

The AM agent owns term-segment encoding/query evaluation and the reloption. Payload
is independent of page envelope: sorted term dictionary entries each reference a
version-1 `plumb_postings::codec` frame. No surrogate document IDs. Postings retain
256-page groups, page masks and sparse tuple offsets. Boolean operations operate
on Postings, then return exact item pointers with `tbm_add_tuples(..., recheck=true)`.

## Query scope

Prune using normalized Term, positive AND/OR/conjunction and boosts that do not
change match semantics; add other operators only if proven safe. Unsupported
NOT/phrases/proximity/fuzzy/regex/min-should-match forms retain the inherited all-
heap-page recheck fallback. Never approximate NOT by subtracting stale CTIDs.
Multiple scan keys intersect independent safe candidate sets. Normal planner
rescan/parameterized query behavior must be preserved.

For multiple segments, union each term's postings across segments **before** Boolean
combination, so a tuple's terms present in different segment histories cannot
produce a missing positive result. Bounds/checks must also cover query text/nodes
and candidate materialization. Fail closed rather than exceeding hard limits.

## Demonstration and gates

1. Load a deterministic corpus spanning multiple 256-page groups before building.
2. Build an opt-in postings index and report nonzero relation storage and segment
   metadata; actual term frames use the tested scalar codec.
3. EXPLAIN ANALYZE SELECT for a rare term must show exact bitmap candidates and far
   fewer heap blocks than the table, not a lossy bitmap covering every page.
4. Compare full IDs with sequential operator evaluation for term/AND/OR, fallback
   queries, NULL/empty documents, expressions/partial predicates and mutations.
5. Exercise insert/update/delete/rollback/HOT/VACUUM/CTID reuse, reopened connections,
   both storage-reloption toggles, reindex/truncate or explicit rejection, supported
   analyzer settings, rejected concurrency/temporary/unlogged cases.
6. Stop immediately and restart a disposable WAL-enabled cluster; compare results
   and validate metadata. Keep hosted-TIN and production-recovery claims separate.
