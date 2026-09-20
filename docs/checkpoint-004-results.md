# Checkpoint 004: default persisted engine, batched builds and bounded merge

**Still a proof of concept.** This checkpoint makes `postings_v1` Plumb's default,
removes the former single-builder whole-table ceiling, and adds a bounded manual
segment consolidation operation. It is not a production compactor or a claim that
all large or high-vocabulary workloads are supported.

## Default SQL

```sql
CREATE EXTENSION plumb;
CREATE INDEX documents_plumb_idx ON documents USING plumb(body);
SELECT id FROM documents WHERE body ~~> 'beacon';

-- Explicit compatibility mode, including for legacy temp-table/analyzer fixtures:
CREATE INDEX documents_heap_idx ON documents USING plumb(body) WITH(storage='heap');

-- Index owner, inherited owning role, or superuser:
SELECT plumb.merge_index('documents_plumb_idx'::regclass);
```

Both a missing reloptions object and unrelated reloptions select the persisted
engine. Existing metapages remain authoritative even after a setting changes.
Old zero-page indexes with no explicit storage setting now fail closed on scan or
insert: deliberately REINDEX into postings, or set explicit heap mode. These 0.1.0
checkpoints have no packaged extension upgrade path; use fresh disposable databases,
not replacement files over an already loaded development installation.

Permanent heaps, canonical text-search semantics, deterministic collation and the
default analyzer are still required by the persisted path. Temporary/unlogged tables,
nondefault analyzers and concurrent index builds are not silently routed to another
engine; request heap mode explicitly where its compatibility behavior is wanted.

## Batched build and larger demonstration

The build initializes metadata once, prepares each document atomically, and flushes
bounded immutable segments. Soft targets are 65,536 pairs or 4 MiB logical storage;
whole-document admission also checks a safe incremental encoded-wire bound. A
prepared document is reused across a flush rather than retokenized or half-added.
The hard 262,144-pair limit now applies to one batch/document, not the whole table.

A PostgreSQL 17.10 release-build fixture loaded **200,000 rows** before a normal
no-WITH CREATE INDEX. Four common terms, parity terms, a rare marker and one extra
marker yield **1,000,201 distinct term/CTID pairs**—well beyond the old whole-build
262,144 limit.

| Observation | Result |
| --- | ---: |
| Default storage | postings_v1, format 1 |
| Bulk segments | 16 |
| Stored logical payload | 2,032,885 bytes |
| Physical relation blocks | 261 |
| Rare beacon matches | 200 |
| Rare AND copper matches | 100 |
| Rare OR markerone matches | 201 |
| Rare exact heap blocks | 200 |
| Rare lossy heap blocks | 0 |

Full IDs matched sequential evaluation and independent arithmetic oracles.
Unrelated-options defaults and explicit heap mode were separately verified.

Individual instrumented samples recorded a **202.367 ms index build** and a rare
query at **17.454 ms indexed versus 1,072.826 ms sequential**. These are synthetic
single-run observations, not controlled repeated benchmarks or a comparison with
hosted TIN. The stronger result is that bounded multi-batch construction and exact
results worked beyond the former cumulative build cap.

## Manual merge: what it does and does not do

`plumb.merge_index(regclass)` returns `changed`, `before` and `after` JSON metadata.
It validates the index, caller ownership and transaction/recovery state. The public
regclass argument does not pre-open the index: it resolves the parent, locks heap
before index and revalidates the relationship. This fixes the index-before-heap
lock inversion found during independent review.

The storage layer holds the metapage's exclusive content lock while capturing and
validating bounded inputs, merging their term sets, writing a fresh standalone
segment, and WAL-publishing its new head/counters. All old segment pages stay
immutable and allocated, so previously captured physical heads remain traversable.
All CTIDs are retained except exact duplicates—there is no current-visibility or
VACUUM-based pruning. This preserves older snapshots and uncommitted candidate
histories. A completed physical merge is not undone by a later SQL transaction
abort; safety relies on the replacement's equivalent candidate union.

Measured merge example after updates/deletes/inserts:

| Metadata | Before | After |
| --- | ---: | ---: |
| Active segments | 60 | 1 |
| Active payload bytes | 13,020 | 546 |
| Physical relation blocks | 61 | 62 |

**Physical size grows; this is not disk-space reclamation.** REINDEX remains the
only tested mechanism to rebuild away retained histories/orphans. Zero or one
active segment returns an unchanged no-op; it is not an integrity scrub.

Limits are deliberate: at most 16 MiB aggregate encoded input, 16 MiB output and
262,144 cumulative decoded input term/CTID pairs, **including duplicates**. The
200k-row fixture exceeds that merge pair budget. Its merge returned the expected
capacity error; all exported metadata and physical size were unchanged and all
rare/AND/OR results still matched their oracles afterwards. An index can therefore
be valid and queryable while too large for this all-active-segments merge. Future
subset merges are needed; the current code does not quietly exceed its limit.

## Correctness and concurrency evidence

- **109 extension tests passed** on PostgreSQL 17: 58 PostgreSQL tests and 51
  host-side tests. Defaults, permissions, real owner/inherited-owner merges,
  wrong-type/format rejection, dropped/recreated OIDs, wire admission and corrupted
  merge inputs are covered.
- **377 pure Rust tests passed**; all 32 postings tests/doctests also pass in release.
  Workspace PG17 Clippy with all targets/pg_test and warnings denied, formatting,
  and full PG18 compilation using shipped bindings pass. PG18 runtime not tested.
- Default SELECT demo: 48 checks. Growth/default/cap-refusal script: 47 checks.
  Existing mutation suite: 346 checks. Prebuild HOT/partial-reuse: 16 checks.
  Public Lead coexistence: 54 checks. Explicit-heap temp-table baseline also passes.
- An anchored repeatable-read snapshot sees **preexisting old term versions** after
  another writer changes text/deletes/inserts and merge replaces the segment head;
  fresh snapshots see the new expected IDs. Both use matching sequential/index IDs.
- An appender publishes uncommitted candidates and waits inside an active INSERT;
  the merger is observed active concurrently, publishes while the writer remains
  open, then the writer appends to the new head. Commit and abort rounds retain
  correct visible results. This proves overlapping statements and serialized head
  publication, not parallel work inside the critical section.
- Two overlapping mergers produce one real merge and one no-op. A later insertion
  remains discoverable. Final main concurrency fixture: **55 rows** (25 oldterm,
  20 newterm, 5 committedmarker, 0 abortedmarker, 5 continuedmarker, 55 sharedterm).
- Deterministic DDL interleaving: one session holds heap ACCESS EXCLUSIVE, a merger
  waits on the heap with **no index relation lock/request**, and the first session
  TRUNCATEs/commits. Merge then sees an empty index and completes without deadlock.
  Subsequent inserts work. This tests the review-driven heap-first fix directly.

## Crash/restart verification

The same disposable cluster was stopped immediately and restarted, with fsync,
full_page_writes and data checksums enabled. No index rebuild or corpus reload was
performed. Startup reported automatic recovery and WAL redo from `0/17297D0` through
`0/650ACC0`.

The merge runner's read-only `--verify-only` mode rechecked the committed manifest,
all expected ID sets and plans, and unchanged metadata. The 200k-row index retained
16 segments / 2,032,885 bytes / 261 blocks. The final merged concurrency index
retained 1 active segment / 907 bytes / 80 physical blocks. The default demo, growth
queries and mutation validation also passed after restart. This is evidence for a
completed merge and later appends surviving local WAL replay—not a publication-
boundary fault campaign, replication/PITR proof or arbitrary power-loss guarantee.

## Review-driven changes and remaining limits

The independent review identified an actual DDL lock inversion, an encoded-size
admission gap and a defensive maintenance-boundary gap. They were fixed and tested:
heap-first locks; incremental safe wire-size admission (including a 60k+120k unique-
term fixture); and PostgreSQL physical tuple-offset validation for every decoded
merge frame, even nonqueried terms. No heap visibility filtering was introduced.

Remaining boundaries:

- Inserts still append one immutable segment each. No mutable ingestion buffer,
  background worker, subset merge, dead-posting reclamation or generation GC.
- Merge stalls appenders while holding its metapage lock. PostgreSQL may defer
  interruption under that content lock; cancellation is not guaranteed to be prompt.
- Aggregate merge input is bounded but decoded maps/output/container overhead add
  memory. Bounds are not work_mem, RSS or a complete process allocation guarantee.
- Whole-index caps remain 64 MiB active payload, 65,536 active segments and 256 MiB
  physical storage including retained history/orphans. Physical capacity can bind
  first. Repeated merging can exhaust it despite a small active payload.
- Queries still have 1,024-byte, 64-depth, 256-node/key/term and 262,144 materialized
  pair limits. Two common terms in the 200k fixture may exceed the query budget
  even when their final intersection would fit. Queries still traverse active
  segments and scoring remains heap-backed.
- Unsupported phrases/NOT/expansions retain heap rechecks. No positions/top-k,
  million-row or high-cardinality benchmark, long-run concurrency stress, online
  concurrent index build, replication/PITR or production upgrade support.
- No forced crash at each replacement-page/publication boundary or physical-head
  traversal barrier test. Old SQL snapshot correctness is tested, but is not itself
  a paused reader holding an old storage-head snapshot. No full physical-cap stress.

## Reproduction and retained evidence

Use a **fresh local disposable database**, follow [tests/postings-demo.md](../tests/postings-demo.md),
then run [default_engine_growth.sql](../tests/default_engine_growth.sql) and
[merge_concurrency.py](../tests/merge_concurrency.py). The latter supports read-only
`--verify-only` after an explicitly authorized restart. Do not overwrite existing
fixtures. The owner-gate regressions are in the pgrx suite.

- [Summary JSON](evidence/checkpoint-004-summary.json)
- [Growth evidence, JSON Lines](evidence/checkpoint-004-growth.jsonl)
- [Merge concurrency and plan evidence](evidence/checkpoint-004-merge.json)
- [Post-restart merge verification](evidence/checkpoint-004-merge-recovery.json)
- [Recovery log excerpt](evidence/checkpoint-004-recovery.txt)

The ZIP contains the full source tree and `.git` history, not database files or an
installed binary. `checkpoint.json` identifies its exact revision. The maintainer
warning, upstream notices and AGPL-3.0-or-later license remain prominent and intact.
