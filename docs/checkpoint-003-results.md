# Checkpoint 003: a real SELECT using persisted grouped CTIDs

**Proof of concept, not production-ready.** This checkpoint meets the narrow
milestone: a PostgreSQL `SELECT` reads an actual persisted term index using the
vision's 256-page CTID grouping, rather than emitting every heap page. It does not
complete the vision's ingestion, maintenance, ranking or operability work.

## What ran

PostgreSQL 17.10, Rust 1.96.0, pgrx 0.19.1, x86-64 Linux. The SQL demonstration used
a non-test **release** build in a newly initialized, disposable cluster with
`fsync=on`, `full_page_writes=on`, `synchronous_commit=on` and data checksums enabled.
No hosted PlanetScale/TIN instance or private source was used.

```sql
CREATE EXTENSION plumb;
-- Load the permanent test corpus first (tests/postings_demo.sql).
CREATE INDEX docs_postings ON plumb_postings_demo.docs
USING plumb (body) WITH (storage = 'postings_v1');

SELECT id
FROM plumb_postings_demo.docs
WHERE body ~~> 'beacon';
```

The corpus contains 30,000 generated ~1 KiB documents plus one NULL and one empty
document. The rare term occurs every 997 IDs. PostgreSQL **naturally selected** a
Bitmap Heap Scan; a separate forced-plan test validates the path explicitly.

## Observed results

| Measurement | Observed |
| --- | ---: |
| Total rows | 30,002 |
| Heap blocks | 4,286 |
| Rare query matches | 30 |
| Exact heap blocks visited | 30 |
| Lossy heap blocks | 0 |
| Immutable bulk segments | 1 |
| Stored logical payload | 356,427 bytes |
| Physical index MAIN-fork blocks | 45 |

Actual IDs are `997, 1994, ... , 29910`. Full results matched both sequential
operator evaluation and the independent arithmetic oracle. The query spans
multiple 256-page groups. A same-page/disjoint-offset case also passes: AND yields
zero, OR yields the two correct tuples, demonstrating tuple—not just page—identity.

A fresh post-recovery natural-plan run produced this excerpt:

```text
Bitmap Heap Scan on docs (... actual time=4.003..4.877 rows=30 loops=1)
  Recheck Cond: (body ~~> 'beacon'::text)
  Heap Blocks: exact=30
  -> Bitmap Index Scan on docs_postings
       (... actual time=3.912..3.912 rows=30 loops=1)
       Index Cond: (body ~~> 'beacon'::text)
Execution Time: 4.954 ms
```

The initial paired instrumented sample recorded **3.696 ms indexed vs 629.889 ms
sequential**, and its separate natural indexed sample recorded 3.764 ms. These are
individual observations on a tiny-vocabulary synthetic fixture, **not a controlled
repeated benchmark**, a capacity estimate or a comparison against hosted TIN.
Caches, query order and instrumentation affect timings. The robust result is exact
candidate pruning: 30 heap pages instead of 4,286 while retaining correct IDs.

## Which parts of the storage vision are implemented

- **Physical CTIDs**, not a surrogate document-ID mapping. Build uses the HOT-root
  TID supplied by PostgreSQL's index build callback.
- **Per-term grouped postings**: the tested v1 scalar codec stores 256-page groups,
  four-word page masks and sorted sparse tuple offsets. Set operations use these
  groups; unions across segments occur before Boolean query combination.
- **A sorted term dictionary per immutable segment**. A CRC32C envelope covers
  term names, lengths and all codec frames, not just posting payloads.
- **PostgreSQL-managed storage**: versioned block-zero metapage and linked immutable
  segment pages in the index MAIN fork. Standard page headers and additional
  envelope checksums (IEEE CRC32 at page envelope level) are validated.
- **Generic WAL**: write/full-image-log every segment page before publishing its
  head/counters in the metapage. Appenders serialize on the metapage buffer lock.
  Readers snapshot the head under a shared lock, then traverse immutable pages.
- **Real `ambuild`, `aminsert` and `amgetbitmap` work**. Positive term/AND/OR and safe
  boost forms produce exact CTIDs via `tbm_add_tuples(..., recheck=true)`.
- **PostgreSQL owns visibility**: every candidate is rechecked. Aborted/deleted/
  reused CTIDs may remain stale positive postings; they are never treated as proof
  of visibility. NOT is not implemented by subtracting stale postings.
- **Inspection**: `plumb.index_stats(index_regclass)` reports format, segments,
  logical bytes and relation blocks. This is metadata inspection, not a full
  validate-index/health API.

## Validation completed

- **73 extension tests passed**: 42 PostgreSQL tests plus 31 host-side tests.
- **377 pure Rust tests passed**; 32 postings tests/doctests passed in release.
- Workspace PG17 Clippy (all targets, pg_test) passed with warnings denied;
  formatting passed. Full PG18 compilation against shipped bindings passed;
  no PG18 server execution is claimed.
- **48 demo checks**: natural/forced plans, full ID multisets, arithmetic oracles,
  same-page offsets, absent term, NULL/empty docs, phrase/NOT/wildcard fallback.
- **346 mutation checks**: insert, indexed/nonindexed update, rollback, delete,
  VACUUM/reuse, reloption changes, REINDEX, TRUNCATE and reinsertion. REINDEX and
  TRUNCATE succeeded, not merely rejected. Runtime pgrx tests additionally cover
  partial/expression indexes, multiple keys, rescans and analyzer changes.
- **16 edge checks**: table statistics prove a HOT update before building the
  index; a separate fixture proves exact CTID reuse outside a partial predicate.
  Both exact-term and wildcard fallback rechecks suppress that stale candidate.
- **54 public Lead coexistence checks** rerun successfully on this build.
- **Concurrent writers**: two different active transactions overlap inside INSERT,
  with advisory gates proving overlap. An anchored repeatable-read reader sees
  zero before/during/after their commits; a new snapshot sees exactly 200 rows.
  A rolled-back 20-row transaction leaves 20 index candidates but zero visible
  rows; subsequent matching inserts work. Final fixture has 210 rows and 14 beacon
  hits. This is a short deterministic test, not sustained concurrency stress.
- **Unsupported operation rejection**: actual CREATE INDEX CONCURRENTLY outside a
  transaction is rejected by Plumb. Nondefault analyzer, temporary/unlogged heaps,
  incompatible text/integer opclasses and foreign query semantics fail cleanly.
  The opclass validation guard was added after independent review found the unsafe
  assumption that any strategy-1 operator/input could be interpreted as text search.

## Immediate-stop recovery evidence

The same disposable cluster was stopped with `pg_ctl ... -m immediate` and restarted.
No corpus reload, CREATE INDEX or REINDEX was performed. Startup logged:

```text
database system was not properly shut down; automatic recovery in progress
redo starts at 0/17297D0
redo done at 0/3F8F470
```

The captured pre-stop WAL insertion position was `0/3F8F4A8`; recovery reached that
end region. Both untouched bulk-index and concurrent-index metadata were identical
before/after: bulk 1 segment / 356,427 bytes / 45 blocks; concurrent 230 segments /
53,856 bytes / 231 blocks. The concurrent chain includes aborted insert postings.

After restart, the original demo passed all 48 checks, mutation revalidation passed
28 checks, and the concurrent committed/aborted IDs and exact positive queries
matched their oracles. PostgreSQL replayed WAL and returned the same results; this
is meaningful local evidence, **not proof of replication, PITR, arbitrary power-loss
scenarios or every cancellation/publication boundary**.

## Important limits and unfinished work

- Enable explicitly with `storage='postings_v1'`. Default `heap` mode remains the
  inherited compatibility baseline. Existing persisted metadata cannot be bypassed
  by toggling the reloption; changing formats requires REINDEX.
- The full vision calls for bounded mutable ingestion and compressed immutable
  segments. This milestone instead appends **one small immutable segment per
  inserted document**. It is simple and costly; every query traverses all committed
  segments and verifies their payloads. Dictionary extents are not yet selectively
  fetched by term, and page-offset encoding is sparse rather than adaptive.
- Default analyzer/canonical text-search semantics/deterministic collation only.
  Positions, fuzzy/regex/NOT and advanced forms fall back to all-heap rechecks.
  Scoring/highlighting remain heap-backed; no top-k or index-backed BM25.
- Hard caps: 16 MiB document/segment; 262,144 build term/CTID pairs; 64 MiB committed
  payload; 65,536 segments; 256 MiB physical index (including orphan pages).
  Queries also cap text at 1,024 bytes, nesting at 64, nodes/keys/terms at 256 and
  materialized term/CTID pairs at 262,144. Conservative limits may reject a query
  that could fit after deduplication. No external-memory spill/work_mem accounting.
- Vector/BTreeMap allocator overhead, transient copies and encoding buffers are
  additional; limits are not an RSS guarantee. Capacity/resource exhaustion testing
  remains incomplete even though bounds exist in code.
- VACUUM does not reclaim dead posting bytes, and no liveness masks, merge,
  background worker, generation reclamation or orphan-page garbage collection exist.
  REINDEX is the only tested way to rebuild/compact the index.
- Cost estimates are inherited/coarse: the demo's estimated rows differ substantially
  from actual rows. Do not generalize its plan choice to other workloads.
- No production upgrade path or stable storage support promise. Use a fresh
  disposable database for each 0.1.0 checkpoint. No million-row/large-vocabulary
  benchmark, hosted TIN parity, ARM/PG18 runtime, exhaustive fuzzing, replication,
  PITR, standby promotion, CLUSTER/VACUUM FULL campaign or publication-fault campaign.

## Evidence files and reproduction

- [Summary JSON](evidence/checkpoint-003-summary.json)
- [Actual SELECT and EXPLAIN output](evidence/checkpoint-003-select.txt)
- [Recovery log excerpt](evidence/checkpoint-003-recovery.txt)
- [Complete in-database plan/check evidence, JSON Lines](evidence/checkpoint-003-db.jsonl)
- [Run guide](../tests/postings-demo.md), [demo SQL](../tests/postings_demo.sql),
  [mutation SQL](../tests/postings_mutations.sql),
  [concurrency runner](../tests/postings_concurrency.py),
  [HOT/reuse edge SQL](../tests/postings_edges.sql),
  [post-recovery SQL](../tests/postings_recovery.sql)

The full-history ZIP checkpoint contains these sources and evidence, not the
PostgreSQL data directory or a runnable installed binary. Its `checkpoint.json`
identifies the exact committed source revision. See [checkpoints.md](checkpoints.md)
for safe extraction and merging; do not overwrite another checkout's `.git`.
