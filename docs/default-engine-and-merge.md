# Checkpoint 004 implementation contract: default engine, batched build, merge

Plumb's default becomes `postings_v1`. The old heap-backed behavior remains explicit
`WITH (storage='heap')`. Existing persisted metapages remain authoritative. A legacy
zero-block index without an explicit heap option is not silently adopted under the
new default: scans/inserts must fail and instruct REINDEX or an explicit heap mode.
This is a development checkpoint, not an in-place extension upgrade promise.

## Batched bulk build

Initialize an empty metapage at the beginning of a nonconcurrent postings build.
Build callback documents feed a bounded per-segment term builder. Flush at a soft
threshold (65,536 term/CTID pairs or 4 MiB logical bytes), or before an incoming document would cross the soft target, or its prepared
projection cannot fit the builder's logical/pair/encoded-wire hard cap. A single document must fit the existing
16 MiB text and 262,144 term/CTID-pair bounds or fail explicitly. A flush produces
an immutable term payload and appends it through the existing WAL publication path.
The final builder is flushed after the heap scan. Table size is no longer bounded
by one builder's pair count, but whole-index payload/segment/physical caps remain.

Preparing a document must be atomic with respect to the builder: tokenization and
projection checks occur before changing builder state, so flushing/retrying cannot
leave a half-added document. Reuse the prepared document across a flush, rather than
parsing limit-error strings or silently dropping terms. Bound preparation itself.
Preserve the existing Builder::add API for insert/unit tests using the same path.

### term_index API additions

```rust,ignore
pub struct PreparedDocument { /* private */ }
pub fn prepare(document: &str, tid: Ctid) -> Result<PreparedDocument, String>;
impl Builder {
    pub fn is_empty(&self) -> bool;
    pub fn should_flush(&self) -> bool;
    pub fn can_accept(&self, document: &PreparedDocument) -> bool;
    pub fn add_prepared(&mut self, document: &PreparedDocument) -> Result<(), String>;
}
pub fn merge_payloads(payloads: &[Vec<u8>]) -> Result<Vec<u8>, String>;
```

A merge validates every input dictionary/frame (including terms not in a query),
unions CTIDs per term and emits canonical existing v1 bytes. Limit cumulative decoded input pairs, including duplicates, to 262,144 and output to 16 MiB. This is a bounded consolidation
operation; exceeding the cap is an error leaving the original index active.
No CTID is discarded for visibility, age or lack of current heap matches: old
snapshots and CTID reuse must remain correct. Duplicate postings can be deduplicated.

## Manual merge: metadata replacement without reclamation

Expose `plumb.merge_index(index_regclass)` returning JSONB with `changed`, `before`
and `after` metadata. Only index owners/appropriate inherited ownership or
superusers may invoke it. The public SQL regclass argument is decoded as an OID,
not an automatically opened PgRelation: resolve the parent, lock heap before index,
then revalidate their association. This avoids index-first deadlock with TRUNCATE. Reject non-Plumb/heap-format indexes and unsupported
relation states before doing storage work. Do not silently convert storage formats.

### storage API addition

```rust,ignore
pub struct MergeResult {
    pub changed: bool,
    pub before: StorageStats,
    pub after: StorageStats,
}
pub unsafe fn merge(
    index: pg_sys::Relation,
    transform: impl FnOnce(&[Vec<u8>]) -> Result<Vec<u8>, String>,
) -> MergeResult;
```

Hold the metapage EXCLUSIVE content lock across snapshot capture, bounded read,
transform and publication. Reuse an internal traversal helper that takes the
captured metadata directly; never call public visit/stats while holding the lock
(they would reacquire it). This serializes appenders. Existing readers holding a
previous head snapshot remain safe because all old pages remain immutable.

At most 16 MiB aggregate input payload is copied for one merge, in addition to the
bounded decoded map/output. For zero or one active segment, return changed=false.
For multiple active segments, reject empty/invalid output or exceeded output/input/
physical limits before publication. Build a replacement segment at newly appended
physical blocks with previous-root NONE, WAL-log its pages, then atomically publish
head/segments=1/payload_bytes in a final WAL record. Never overwrite old segment
pages, truncate, reuse blocks or publish partially written output. Errors/cancellation
before publication leave old metadata active; appended orphans remain bounded.

Physical size can grow after merge. This is **not** VACUUM, compaction/reclamation,
a mutable ingestion layer or the final generation lifecycle. Retaining old pages
is the safety mechanism for existing readers. Capacity reclamation remains REINDEX.

## Validation gates

- Default CREATE INDEX has a metapage and exact positive bitmap results, including
  no-reloptions and unrelated-reloptions forms. Explicit heap mode remains usable.
- Default inserts, expressions, partial predicates, rescan, scoring and coexistence
  retain their prior expected outcomes; pin only deliberately heap-specific tests
  to storage='heap', never weaken assertions to hide regressions.
- Bulk corpus exceeding the old 262,144 total pair ceiling builds multiple segments
  with bounded per-segment builders, and rare/AND/OR IDs equal sequential/oracle IDs.
- Many insert segments merge into one, with identical results before/after, then
  continued inserts work. Duplicate CTIDs across histories are unioned safely.
- Old repeatable-read snapshots, simultaneous appenders and merge, and immediate-
  stop/restart do not lose committed results. Include owner-access rejection.
- Both format and payload codec stay v1; no extra dependency or external files.
- Report restrictions, hard limits, timing context and physical-size growth clearly.
