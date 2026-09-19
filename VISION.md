# Plumb Vision

> Roadmap, not a description of current capabilities. Plumb is a proof-of-concept investigation, not production-ready software. See [README.md](README.md) and [implementation status](docs/implementation-status.md).

## Purpose

Plumb is a production-oriented PostgreSQL full-text search extension derived from PlanetScale Lead and informed by PlanetScale's publicly documented TIN architecture.

The project lives at `strigops-io/plumb`, retain Lead's `AGPL-3.0-or-later` licensing status, preserve upstream copyright and attribution, and aim for practical SQL compatibility with TIN where that can be achieved from public interfaces and independently observed behaviour.

Plumb is not a copy of PlanetScale's private TIN implementation. It is an independent implementation built from:

1. the AGPL-licensed Lead source;
2. PlanetScale's public TIN design documentation;
3. PostgreSQL's documented access-method contracts; and
4. controlled black-box experiments against an authorised TIN instance provided exclusively for this investigation.

The central architectural commitment is simple:

> PostgreSQL's physical tuple identifier, `ctid`, is the posting identity.

Plumb will not introduce a surrogate search document ID that must be mapped back to a tuple. A posting identifies a tuple version by heap block and line pointer, aligning the search index with PostgreSQL's own index and heap machinery.

## Why Plumb Exists

Lead provides a valuable compatibility scaffold: TINQL, tokenisation, index options, operators, scoring behaviour, highlighting, and an access-method shell. It is intentionally not a production search index. It stores no real postings, performs no meaningful insertion or vacuum maintenance, and satisfies scans by returning lossy coverage of the heap for PostgreSQL to recheck.

Plumb will preserve the useful compatibility surface while replacing that deliberately pathological storage path with a durable, observable, maintainable index.

The project succeeds if a PostgreSQL operator can use it as a normal database index:

- exact search results under PostgreSQL MVCC;
- bounded online write cost;
- correct handling of updates, HOT chains, deletes, and VACUUM;
- crash recovery, physical replication, PITR, backup, and restore;
- useful planner estimates;
- inspectable health and maintenance state;
- predictable resource limits;
- a versioned on-disk format and supported upgrade path.

Raw benchmark speed is important, but correctness, durability, and operability are release gates.

## Design Principles

### CTID is the document identity

A PostgreSQL tuple identifier consists of a heap block number and a tuple offset. Plumb uses that identity directly in postings. This removes the document-ID-to-CTID lookup found in conventional external search indexes and naturally groups work by heap page.

CTIDs identify physical tuple versions, not logical rows. Plumb must therefore treat table rewrites, HOT chains, pruning, and VACUUM as core correctness concerns.

### Query from page to tuple

Postings use two levels:

1. a presence bitmap over fixed groups of heap pages; and
2. an offset representation for matching tuples on each present page.

The initial design target is 256 heap pages per group, giving a 256-bit page mask. Query execution first intersects or unions page masks, then decodes offsets only on surviving pages. Sparse and dense offset representations may coexist.

The portable scalar implementation is normative. SIMD implementations are optional accelerators selected at runtime and must produce identical results.

### PostgreSQL remains the visibility authority

The index produces candidate CTIDs. PostgreSQL heap visibility decides whether a tuple is visible to the current snapshot unless a proven, safe optimisation applies.

Visibility-map fast paths may avoid heap work only when PostgreSQL guarantees permit it. Per-segment liveness state records CTIDs that VACUUM has proved dead; it is not a substitute for snapshot visibility.

### Segments separate ingestion from search

Online writes enter bounded mutable segments. Maintenance seals them into compressed immutable segments. Queries see a snapshot-safe union of active mutable and immutable segments.

Immutable segments may be merged without renumbering documents because CTIDs retain their meaning across segments. Publication and replacement use a crash-safe generation protocol. Superseded storage is reclaimed only after no backend can still reference it.

### PostgreSQL owns storage and durability

Index state belongs in PostgreSQL-managed relations or forks, not an uncoordinated side directory. All visible state changes must be WAL-logged or safely reconstructable. Standard PostgreSQL backup, recovery, replication, checksum, locking, and buffer-management expectations apply.

### Compatibility is measured, not assumed

TIN SQL and TINQL compatibility are valuable, but undocumented behaviour will not be guessed into a permanent contract. Plumb will maintain a compatibility matrix distinguishing:

- inherited behaviour verified from Lead;
- behaviour documented publicly by PlanetScale;
- behaviour observed through authorised black-box tests;
- intentional Plumb extensions or differences.

## Primary SQL identity and coexistence

Plumb's primary extension, library, schema and access method are named `plumb`.
Search uses `~~>`; the canonical operator is `pg_catalog.~~>(text,text)`, bound to
Plumb's own function and operator class. Scoring/highlighting use `plumb.*` and
bind only to that exact operator OID and Plumb indexes. Existing Lead/TIN `tin`
objects and `pg_catalog.==>` are never replaced or adopted. See
[the coexistence contract](docs/coexistence.md).

A separate optional TIN-name compatibility package is deferred. It cannot coexist
with a real provider claiming the same `tin` extension/schema/access-method names
or `==>` signature. It must refuse collisions, remain independently installable,
and never overwrite another provider's server files. No such package is shipped
in the current milestone.

## Target Architecture

### Reused from Lead

Plumb should retain and test before modifying:

- the TINQL parser and query semantics;
- tokenisers, case folding, accent folding, long-token handling, and position gaps;
- the underlying operator and SQL helper behavior, exposed primarily as `~~>` and `plumb.*`;
- BM25 arithmetic and term-frequency bucketing where behaviour is compatible;
- highlighting as an initial heap-backed implementation;
- the pgrx/PostgreSQL access-method scaffold.

### Replaced or added

The production implementation requires:

- term dictionaries and dictionary enumeration;
- exact CTID postings;
- page-presence and tuple-offset encodings;
- position streams for phrase, near, before, and span queries;
- persistent document, length, document-frequency, and posting statistics;
- immutable builds and mutable ingestion;
- real `aminsert`, `amgetbitmap`, scan, vacuum, and cost-estimation paths;
- VACUUM-driven liveness maintenance;
- segment sealing, merging, publication, and reclamation;
- WAL and recovery;
- snapshot-safe generation pinning;
- wildcard, regex, and fuzzy term expansion with hard limits;
- index-backed scoring and eventual top-k execution;
- observability, validation, checksums, and disk-format versioning.

### Indicative repository structure

```text
plumb/
├── postgres/
│   ├── am/          # build, insert, scans, vacuum, costing, parallel paths
│   ├── storage/     # metapage, directory, segments, postings, WAL, liveness
│   ├── query/       # compilation, Boolean execution, phrases, fuzzy, top-k
│   └── worker/      # flush, merge, garbage collection
├── tinql/           # inherited from Lead
├── tokenizer/       # inherited from Lead
├── boldi-vigna/     # inherited or adapted
├── benches/
├── fuzz/
└── docs/
```

The final layout should follow the upstream repository where that reduces fork maintenance.

## Storage Model

Every index starts with a versioned metapage containing a magic value, index-format version, feature flags, and the current committed segment-directory generation.

A segment directory identifies active segments and their heap-page range, statistics, storage roots, generation, and state. Immutable data is split into independently addressable extents for dictionaries, page masks, offsets, frequencies, positions, and liveness.

A segment follows an explicit lifecycle:

```text
BUILDING → SEALED → ACTIVE → SUPERSEDED → RECLAIMABLE
```

Only committed ACTIVE segments are queried. Publication must be atomic from the perspective of a backend opening a scan. Incomplete BUILDING state is ignored and reclaimed after recovery. Old generations remain accessible while pinned by scans.

The index format and extension version are separate. A compatible extension upgrade must not silently reinterpret an incompatible disk format.

## Query Execution

The initial executor will support exact Boolean search:

1. compile TINQL to a bounded internal query tree;
2. resolve terms through mutable and immutable dictionaries;
3. combine page masks;
4. decode and combine offsets only for surviving pages;
5. apply liveness masks;
6. return exact CTIDs to PostgreSQL;
7. allow the heap to enforce snapshot visibility and any required operator recheck.

Positions, frequencies, scoring, and top-k are layered on this base:

- phrase and proximity queries intersect term postings before reading position streams;
- Boolean-only queries do not decompress positions;
- BM25 reads persistent corpus and per-term statistics;
- a later ranked executor uses WAND, block-max WAND, MaxScore, or an evidence-supported equivalent rather than scoring every match.

Queries that expand vocabulary, including wildcard, fuzzy, and regex forms, must be bounded by configurable limits on query nodes, expanded terms, automaton states, and positions examined.

## PostgreSQL Correctness Contract

Plumb must demonstrate correctness for:

- inserts, committed and aborted transactions;
- non-HOT and HOT updates;
- deletes under short and long-running snapshots;
- VACUUM, pruning, freezing, and visibility-map changes;
- TRUNCATE;
- VACUUM FULL, CLUSTER, and other heap rewrites;
- CREATE INDEX and REINDEX, including concurrent variants before 1.0;
- partitioned tables;
- NULL, empty, zero-token, and TOASTed documents;
- checkpoint, immediate stop, crash recovery, standby replay, promotion, and PITR.

When a safe fast path is unavailable, Plumb must choose a slower PostgreSQL-correct path.

## Operations and Observability

The extension should expose stable inspection functions, for example:

```sql
SELECT * FROM plumb.index_stats('documents_body_idx');
SELECT * FROM plumb.segment_stats('documents_body_idx');
SELECT plumb.validate_index('documents_body_idx', 'metadata');
```

Operators should be able to see segment counts, mutable backlog, document and posting counts, dead-posting ratio, storage by component, pending merge work, last successful maintenance, format version, and validation status.

Long-running build, merge, vacuum, dictionary, and decode loops must be interruptible. Memory must be charged to appropriate PostgreSQL memory contexts and constrained by `work_mem`, `maintenance_work_mem`, or documented Plumb GUCs.

## Security and Independent Implementation

The supplied TIN instance exists only for the Plumb investigation and its test programme; it is not a production or shared-workload system. Its database contents may be created, mutated, vacuumed, rewritten, and discarded as required by the approved experiments. Restart, crash, replication, filesystem, and host-level tests remain subject to the capabilities and permissions actually provided.

Investigation of TIN is restricted to authorised black-box observation through normal PostgreSQL interfaces and permitted host-level telemetry. The project will not seek or incorporate proprietary source, binaries extracted for reverse engineering, secrets, private symbols, or confidential internal documentation.

Every compatibility claim should carry provenance in an evidence ledger. Findings should describe observable inputs and outputs, not speculate that an internal implementation is identical to Plumb's.

Lead-derived files retain their notices and AGPL licensing. New project code is also licensed `AGPL-3.0-or-later`. Dependencies must be compatible with distribution under that licence.

## Delivery Plan

### Phase 0  Fork and baseline

- fork Lead and preserve provenance;
- use the independent `plumb` package/extension/schema/access method and `~~>` operator;
- coexist with existing Lead/TIN without modifying its objects; defer optional TIN-name packaging to a later phase;
- run upstream compatibility tests unchanged;
- establish CI across supported PostgreSQL versions;
- record baseline plans, results, and performance.

### Phase 1  Read-only exact index

- versioned metapage and immutable segment directory;
- bulk build into sparse exact CTID postings;
- two-level 256-page grouping;
- scalar Boolean operations;
- exact bitmap scan with heap visibility.

Exit criterion: exact term and Boolean results match a sequential reference implementation across generated corpora.

### Phase 2  Positions and scoring

- frequencies and positions in separate streams;
- phrase and proximity execution;
- persistent corpus statistics;
- index-backed BM25 compatible with the retained SQL API.

### Phase 3  Online mutation and vacuum

- bounded mutable segments and `aminsert`;
- commit, abort, update, HOT-chain, and delete correctness;
- VACUUM callbacks and liveness maps;
- safe visibility-map optimisations.

### Phase 4  Durability and maintenance

- WAL for all persistent mutations;
- recovery-safe segment publication;
- segment flush and merge;
- generation pinning and reclamation;
- optional background workers, with manual maintenance remaining available.

Exit criterion: repeated crash, restart, standby, and PITR tests produce no missing or false-visible results.

### Phase 5  Planner and advanced execution

- statistics-based selectivity and costing;
- bounded wildcard, regex, and fuzzy dictionary expansion;
- count optimisations;
- SIMD runtime dispatch;
- top-k execution;
- parallel builds and selected parallel queries.

### Phase 6  Production release

- CREATE INDEX CONCURRENTLY and REINDEX CONCURRENTLY;
- stable introspection and validation;
- corruption detection and checksums;
- documented format-upgrade policy;
- backup, restore, replication, and operational runbooks;
- fuzzing, fault injection, soak tests, and compatibility matrix.

## Release Standard

Version 1.0 means production-supportable, not merely feature-complete. It requires:

- no known MVCC, HOT, rewrite, or VACUUM correctness gaps;
- demonstrated crash safety and replay;
- supported concurrent build and reindex;
- bounded memory and adversarial query behaviour;
- on-disk format versioning and validation;
- actionable operational telemetry;
- reproducible performance and correctness suites;
- clear compatibility and upgrade documentation.

SIMD, sophisticated ranking, or a benchmark headline cannot compensate for missing durability or correctness.

## Explicit Non Goals

Plumb will not:

- embed Tantivy, Lucene, or another engine as its primary storage model;
- add a surrogate document ID to map back to CTID;
- require SIMD-capable hardware;
- promise undocumented TIN internals are reproduced;
- make background workers mandatory for basic use;
- bypass PostgreSQL MVCC or persistence rules for speed;
- claim production readiness before WAL, recovery, VACUUM, and rewrite tests pass.

## Source Material

- PlanetScale, [Introducing TIN](https://planetscale.com/blog/introducing-tin)
- PlanetScale, [Lead repository](https://github.com/planetscale/lead)
- PostgreSQL documentation, [Index Access Method Interface Definition](https://www.postgresql.org/docs/current/indexam.html)
- PostgreSQL documentation, [Index Access Method Functions](https://www.postgresql.org/docs/current/index-functions.html)
- PostgreSQL documentation, [Visibility Map](https://www.postgresql.org/docs/current/storage-vm.html)
