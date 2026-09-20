# TIN Behaviour Investigation Plan

## Objective

This document is the starting protocol for an agent investigating an authorised PostgreSQL instance running PlanetScale TIN.

The supplied PostgreSQL instance belongs exclusively to this investigation and its tests. It is not a production instance and carries no workload or data that needs to be preserved. Test schemas, tables, indexes, transactions, and database state may therefore be created, mutated, rewritten, truncated, dropped, or recreated as the investigation requires.

The goal is to characterise observable behaviour that can guide an independent implementation of Plumb. The investigation must distinguish facts from hypotheses, preserve reproducible evidence, and avoid unsupported claims about TIN internals.

The primary outputs are:

- a catalogue of the SQL and access-method surface;
- a behavioural compatibility matrix;
- reproducible experiments covering query, mutation, MVCC, maintenance, and recovery;
- evidence-backed constraints for Plumb's design;
- a list of unresolved questions and the next cheapest experiment for each.

## Investigation Boundaries

The supplied instance is dedicated to these tests. Database-level destructive operations are permitted when they serve a documented experiment, including dropping or recreating probe objects, truncating test data, forcing table rewrites, and rebuilding indexes. Before each such operation, capture any evidence needed for comparison and record the mutation in the experiment ledger.

Only use systems and privileges explicitly authorised by the instance owner. A dedicated test instance does not imply permission to access its host, retrieve proprietary implementation material, change surrounding infrastructure, or affect any other system.

Allowed by default when permission exists:

- PostgreSQL catalog queries;
- extension-provided SQL functions and documented GUCs;
- `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL)`;
- creation, mutation, rewriting, truncation, and destruction of test schemas, tables, indexes, and data;
- standard extensions such as `pageinspect`, `pgstattuple`, and `pg_visibility`;
- controlled sessions and transactions;
- documented restart, checkpoint, backup, restore, replication, and crash tests when the supplied environment exposes and authorises those controls.

Out of scope unless separately authorised:

- any system, database, or data outside the supplied dedicated test instance;
- filesystem or memory scraping;
- extracting or disassembling proprietary binaries;
- searching for credentials, private symbols, source, or confidential internal material;
- bypassing access controls;
- publishing instance identifiers, secrets, or non-public implementation details.

Treat TIN as a black box. An observation such as “index relation size increases by one page” is evidence. “TIN stores X in that page” remains a hypothesis until a permitted interface establishes it.

## Required Access

### Minimum useful access

- connect with two or more concurrent sessions;
- create a dedicated database or schema;
- create tables and TIN indexes;
- query PostgreSQL system catalogs;
- run `EXPLAIN ANALYZE`;
- run `VACUUM`, `ANALYZE`, `REINDEX`, `CLUSTER`, and `TRUNCATE` on probe objects;
- inspect `ctid`, `xmin`, and `xmax`.

### Preferred additional access

- install or use `pageinspect`, `pg_visibility`, and `pgstattuple`;
- call `pg_relation_size` and inspect relation forks;
- change session GUCs;
- create logical or physical replicas;
- checkpoint and restart a disposable server;
- force an immediate stop and recover a disposable server;
- inspect PostgreSQL logs and `pg_stat_*` views.

Record unavailable privileges before testing. Design around them rather than attempting to bypass them.

## Evidence Standard

Every experiment gets a stable ID such as `CAT-001`, `QRY-003`, or `MVCC-007`.

For each run, capture:

```yaml
experiment: QRY-003
timestamp_utc:
investigator:
environment_id:
postgres_version:
tin_extension_version:
server_settings:
preconditions:
sql_script:
expected:
observed:
artifacts:
interpretation:
confidence: low | medium | high
alternative_explanations:
plumb_decision:
follow_up:
```

Store raw output separately from interpretation. Redact connection strings, hosts, usernames, and data values not created by the test.

Use these evidence labels:

- **Documented**: stated in an authoritative public source.
- **Source verified**: present in AGPL Lead source.
- **Observed**: reproduced against the authorised TIN instance.
- **Inferred**: best explanation of observations, with alternatives recorded.
- **Plumb decision**: an independent design choice, not a claim about TIN.

Repeat surprising results at least three times and on a freshly recreated fixture when practical.

## Environment Manifest

Create `evidence/environment.md` before running behavioural tests. Record:

```sql
SELECT version();
SHOW server_version_num;
SHOW block_size;
SHOW wal_level;
SHOW data_checksums;
SHOW shared_preload_libraries;

SELECT name, default_version, installed_version, comment
FROM pg_available_extensions
WHERE name IN ('tin', 'lead', 'pageinspect', 'pg_visibility', 'pgstattuple')
ORDER BY name;

SELECT extname, extversion, extnamespace::regnamespace
FROM pg_extension
ORDER BY extname;
```

Also capture relevant settings without exposing secrets:

```sql
SELECT name, setting, unit, source
FROM pg_settings
WHERE name ~ '^(tin|shared_buffers|work_mem|maintenance_work_mem|max_parallel|autovacuum|track_io_timing)'
ORDER BY name;
```

Record operating system and filesystem only when provided through an authorised interface.

## Probe Schema and Data

Use a unique isolated schema. Do not use `public`.

```sql
CREATE SCHEMA plumb_probe;

CREATE TABLE plumb_probe.documents (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    body text,
    padding text,
    category integer,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX documents_body_tin_idx
ON plumb_probe.documents
USING tin (body);
```

Before assuming syntax or opclass names, complete catalog experiment `CAT-001` and adjust the fixture to the installed API.

Use deterministic synthetic tokens that cannot be confused with natural stop words:

- `rarealpha`, present once;
- `rarebravo`, present on a known second page;
- `commoncharlie`, present in most documents;
- `phrase_delta phrase_echo`, adjacent;
- `phrase_delta gap phrase_echo`, non-adjacent;
- repeated `freqfoxtrot` terms for TF tests;
- variants for case, accent, Unicode, long tokens, wildcard, fuzzy, and regex tests.

Set padding lengths deliberately to force page boundaries. After every load or mutation, capture:

```sql
SELECT id, ctid, xmin, xmax, length(body), length(padding), body
FROM plumb_probe.documents
ORDER BY ctid;

SELECT pg_relation_size('plumb_probe.documents') AS heap_bytes,
       pg_relation_size('plumb_probe.documents_body_tin_idx') AS index_bytes,
       pg_indexes_size('plumb_probe.documents') AS all_index_bytes;
```

Never rely on IDs to infer physical placement; record actual CTIDs.

## Phase 1  Catalog and API Surface

### CAT-001 Access method and operator classes

Capture:

```sql
SELECT * FROM pg_am WHERE amname = 'tin';

SELECT opc.oid, opc.opcname, opc.opcdefault,
       opc.opcintype::regtype, opc.opckeytype::regtype,
       opc.opcfamily, am.amname
FROM pg_opclass opc
JOIN pg_am am ON am.oid = opc.opcmethod
WHERE am.amname = 'tin'
ORDER BY opc.opcname;
```

### CAT-002 Operators and support functions

Join `pg_amop`, `pg_amproc`, `pg_operator`, and `pg_proc` through the TIN operator families. Record signatures, volatility, parallel safety, strictness, and ownership. Do not assume a function's implementation from its name.

### CAT-003 Extension-owned objects

```sql
SELECT e.extname,
       pg_describe_object(d.classid, d.objid, d.objsubid) AS object
FROM pg_depend d
JOIN pg_extension e ON e.oid = d.refobjid
WHERE d.deptype = 'e' AND e.extname = 'tin'
ORDER BY object;
```

### CAT-004 Index capabilities

Record `pg_index`, `pg_class`, `pg_attribute`, index options, reloptions, supported column types, multicolumn behaviour, INCLUDE support, partial indexes, expression indexes, and partitioned indexes.

### CAT-005 Diagnostics and settings

Enumerate only extension-owned functions, views, and `tin.*` GUCs exposed through PostgreSQL. Capture their documented output and permissions.

**Phase output:** `evidence/catalog-surface.md` and machine-readable CSV/JSON extracts.

## Phase 2  Query Semantics and Plans

For every query family, compare TIN results to a deterministic reference evaluated from heap data or Lead's documented semantics. Capture cold and warm runs separately where cache state can be controlled.

### QRY-001 Basic terms and Boolean logic

Test term, AND, OR, NOT, parentheses, absent terms, common terms, and combinations whose expected IDs are known.

### QRY-002 Recheck and scan type

Use:

```sql
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL)
SELECT id, ctid, body
FROM plumb_probe.documents
WHERE body ==> 'rarealpha';
```

Record bitmap versus ordinary index scans, exact and lossy heap blocks, rows removed by index recheck, heap fetches, and buffer access. Repeat with scan methods selectively disabled as a diagnostic, not as a performance claim.

### QRY-003 Page pruning

Place two terms on disjoint pages and then on overlapping pages. Compare buffers and execution work for `AND` and `OR`. Determine whether observations are consistent with page-level pruning; do not claim a specific bitmap encoding solely from timing.

### QRY-004 Phrase and proximity

Test adjacency, reversed order, gaps, repeated terms, position gaps across values if supported, and phrase queries containing frequent terms. Determine whether heap rechecks occur.

### QRY-005 Scoring

Vary term frequency, document length, document frequency, and query multiplicity one factor at a time. Record score APIs and ordering stability. Compare results with Lead's public BM25 arithmetic where applicable.

### QRY-006 LIMIT and top-k

Compare `ORDER BY score DESC LIMIT k` for multiple values of `k` against an unbounded scored query. Capture plans, buffers, time, and score equality. Evidence of sublinear work is suggestive of top-k execution but does not identify its algorithm.

### QRY-007 COUNT

Compare `count(*)` with selecting CTIDs for rare, common, overlapping, and disjoint terms. Repeat before and after VACUUM changes all-visible coverage. Capture heap buffers and `pg_visibility` output if available.

### QRY-008 Wildcard regex and fuzzy

Build a controlled vocabulary and vary dictionary size without changing matching document counts. Record limits, error behaviour, planning time, execution time, memory symptoms, and result semantics.

### QRY-009 NULL Unicode and tokenisation

Cover NULL, empty text, whitespace, zero-token content, case folding, accents, emoji, combining marks, CJK, invalidly long tokens, and TOASTed documents. Record both search behaviour and scoring/statistical effects.

**Phase output:** `evidence/query-matrix.md`, raw plans, and expected-versus-actual result sets.

## Phase 3  Physical Layout and Size Deltas

Physical inspection must use permitted PostgreSQL functions. Raw pages may reveal structure, but field meanings remain hypotheses unless corroborated.

### PHY-001 Relation forks and baseline

```sql
SELECT pg_relation_filepath('plumb_probe.documents_body_tin_idx');

SELECT fork,
       pg_relation_size('plumb_probe.documents_body_tin_idx', fork)
FROM unnest(ARRAY['main','fsm','vm','init']) AS fork;
```

Some forks may not exist; record errors or zero sizes accurately.

### PHY-002 Incremental size experiment

Recreate the index for each controlled corpus:

1. empty table;
2. one document with one unique term;
3. repeated term in one document;
4. same term across increasing tuple offsets on one page;
5. same term across successive heap pages;
6. boundary candidates near 256-page intervals;
7. dense common term;
8. positions-heavy document.

Record heap and index byte deltas, page count, and CTIDs. Repeat to separate deterministic allocation from background maintenance.

### PHY-003 Page inspection

If permitted, use `get_raw_page` and generic page header inspection. Store hashes and narrowly scoped hex samples only if authorised. Identify magic/version values, page types, or references only when repeated controlled deltas support the interpretation.

### PHY-004 Segment manifestation

Insert in measured batches and observe size, catalog, diagnostic, and page changes over time. Test whether explicit maintenance, transaction count, tuple count, bytes, or elapsed time correlates with abrupt changes suggesting segment sealing or merge.

**Phase output:** `evidence/physical-layout.md` with separate observation and hypothesis sections.

## Phase 4  Mutation MVCC and VACUUM

Use at least two sessions and write down the interleaving exactly.

### MVCC-001 Uncommitted insert

Session A inserts a matching document without committing. Session B searches before commit; repeat after commit and after rollback. Capture CTIDs and plans.

### MVCC-002 Snapshot isolation

Hold a REPEATABLE READ transaction in Session B. Commit an insert or delete in Session A. Verify old and new snapshots see PostgreSQL-correct results.

### MVCC-003 Update with indexed value changed

Update a row so the old term disappears and a new term appears. Record old and new CTIDs, HOT status indicators where observable, results under concurrent snapshots, and effects of VACUUM.

### MVCC-004 HOT-eligible update

Update only a non-indexed field while leaving space for HOT. Record `ctid`, page inspection or `pg_stat_all_tables` HOT counters, and search correctness before and after pruning.

### MVCC-005 Delete and long-running snapshot

Keep a snapshot that can see a row, delete it elsewhere, VACUUM while the snapshot remains, then release and VACUUM again. Observe search results, index size, diagnostics, and liveness-related evidence.

### MVCC-006 Aborted update and delete

Confirm aborted mutations never change visible query semantics, including after VACUUM and restart.

### VAC-001 VACUUM effects

Measure before and after ordinary VACUUM, aggressive VACUUM, ANALYZE, and repeated cycles. Capture index size, tuple statistics, visibility map, buffers for COUNT, diagnostics, and plans.

### VAC-002 Rewrite operations

On disposable probe data, test TRUNCATE, CLUSTER, VACUUM FULL, and supported ALTER TABLE rewrites. Verify index validity, CTID changes, whether a rebuild occurs, and search correctness.

**Phase output:** session transcripts and a state table mapping operation, snapshot, CTID, expected visibility, and observed visibility.

## Phase 5  Maintenance Concurrency and Resource Behaviour

### MNT-001 Mutable-to-immutable transition

Insert small committed batches while repeatedly searching. Look for thresholds or scheduled transitions in diagnostics, relation size, WAL volume, latency, and plans.

### MNT-002 Merge concurrency

Run searches and writes while a merge or explicit maintenance operation occurs. Verify result completeness, latency outliers, blocking locks, and generation-like changes.

### MNT-003 Cancellation

Cancel large builds, regex expansion, scans, and maintenance operations. Confirm prompt cancellation, no invalid index publication, and successful subsequent validation/rebuild.

### MNT-004 Memory bounds

Vary `work_mem` and `maintenance_work_mem` on large OR, regex, build, and maintenance workloads. Record clean errors, spilling, backend memory observations through authorised telemetry, and server stability.

### MNT-005 Lock behaviour

Query `pg_locks` during build, insert, VACUUM, maintenance, REINDEX, and DROP. Record conflicts and whether normal DML remains available.

**Phase output:** operational concurrency matrix and candidate Plumb resource limits.

## Phase 6  Planner Costing

### PLN-001 Selectivity

For known term frequencies from 1 row to nearly all rows, record estimated and actual rows. Repeat after ANALYZE and after large corpus changes.

### PLN-002 Compound estimates

Test correlated and independent terms under AND and OR, phrases, wildcard expansions, and negation. Compare estimates with actuals.

### PLN-003 Plan crossover

Grow matching fractions until PostgreSQL changes between index and sequential plans. Record table size, visibility, cache condition, costs, and GUCs.

### PLN-004 LIMIT sensitivity

Determine whether cost and plan choice react to LIMIT and ranked ordering.

**Phase output:** CSV of estimated versus actual cardinality and notes for Plumb's `amcostestimate` design.

## Phase 7  Durability Replication and Recovery

Run only on disposable, owner-authorised infrastructure.

### DUR-001 Clean restart

Commit writes, checkpoint, restart, and verify results, scores, diagnostics, and validation.

### DUR-002 Crash during insert

Force an immediate stop at controlled points around committed and uncommitted writes. Recover and compare against heap truth.

### DUR-003 Crash during build and publication

Interrupt CREATE INDEX and any observed segment seal or merge phases. After recovery, verify the index is valid or clearly invalid and rebuildable; it must not silently return incomplete results.

### DUR-004 Physical standby

Replay builds and mutations to a standby. Compare catalog state, relation sizes, query results, and promotion behaviour.

### DUR-005 Backup restore and PITR

Restore at points before and after index creation, mutation, and maintenance. Verify result sets and index validation at each recovery target.

Capture PostgreSQL logs, WAL metrics, timelines, and scripts. Do not infer that a lack of visible errors proves full crash safety.

**Phase output:** recovery matrix with operation, interruption point, committed heap truth, recovered index result, and status.

## Phase 8  DDL Partitioning and Compatibility

Test:

- CREATE INDEX and CREATE INDEX CONCURRENTLY;
- REINDEX and REINDEX CONCURRENTLY;
- DROP INDEX during active scans;
- partial and expression indexes;
- partition attach, detach, and default partition;
- schema rename, table rename, column rename, and extension update;
- pg_dump schema output and restore;
- supported PostgreSQL major versions.

Each test must say whether the feature is supported, rejected cleanly, or appears to work but has unresolved semantics.

## Automation Layout

A handoff-friendly harness should use this layout:

```text
investigation/
├── README.md
├── config.example.env
├── sql/
│   ├── 00_environment.sql
│   ├── 10_catalog.sql
│   ├── 20_fixture.sql
│   ├── 30_queries.sql
│   ├── 40_mvcc.sql
│   ├── 50_vacuum.sql
│   └── 60_ddl.sql
├── scripts/
│   ├── run-experiment.sh
│   ├── capture-plan.sh
│   └── normalise-output.py
├── evidence/
│   ├── environment.md
│   ├── ledger.yaml
│   ├── plans/
│   ├── results/
│   └── logs/
└── reports/
    ├── compatibility-matrix.md
    ├── findings.md
    └── open-questions.md
```

Secrets belong in environment variables or an approved secret store and must never enter evidence artifacts. Scripts should use `ON_ERROR_STOP=1`, emit timestamps, record transaction boundaries, and make destructive commands require an explicit disposable-environment flag.

Normalisation may remove volatile timing, OIDs, paths, and transaction IDs for diffs, but raw originals must be retained privately.

## Compatibility Matrix

Maintain a table with at least these columns:

| Area | Case | Lead behaviour | TIN observed | Plumb target | Evidence | Confidence | Status |
|---|---|---|---|---|---|---|---|
| Boolean | rare AND common | pending | pending | exact CTIDs | QRY-001 | low | not run |
| MVCC | uncommitted insert | heap recheck | pending | invisible cross-session | MVCC-001 | low | not run |
| Vacuum | dead tuple | no index state | pending | liveness cleared when safe | VAC-001 | low | not run |

Do not turn an observed TIN quirk into a Plumb requirement automatically. Record whether compatibility is essential, desirable, optional, or intentionally rejected.

## Decision Questions

The investigation should resolve or narrow these questions:

1. Which query forms return exact CTIDs, and which require heap recheck?
2. What statistics are persistent and when are they refreshed?
3. Are positions and document lengths demonstrably index-resident?
4. How do write-visible mutable structures manifest through public interfaces?
5. What triggers sealing and merging?
6. What does VACUUM change, and when are dead postings reclaimed?
7. How do HOT chains and pruned line pointers affect matches?
8. Does COUNT avoid heap access on all-visible pages?
9. Are 256-page boundaries observable in allocation or query work?
10. How do sparse and dense term behaviour differ?
11. Does ranked LIMIT avoid scoring all matches?
12. How is planner selectivity related to term statistics?
13. Which operations generate WAL, and how do partial operations recover?
14. What locks and resource limits affect production operation?
15. Which TIN behaviours should Plumb match, and which should remain implementation-independent?

## Agent Working Method

An investigating agent should:

1. inventory access and environment;
2. run catalog discovery before assuming syntax;
3. create only isolated synthetic fixtures;
4. establish expected heap truth for every test;
5. vary one factor at a time;
6. capture raw SQL, output, plans, CTIDs, sizes, settings, and timing;
7. label observation separately from inference;
8. repeat anomalous results;
9. update the compatibility matrix and open-question list after each experiment;
10. propose a Plumb design decision only when the evidence changes a real choice.

The agent may freely mutate or recreate database objects and data within the supplied dedicated test instance when the action is part of a recorded experiment. The agent should stop and ask for authorisation if a proposed test requires broader privileges, reaches beyond that instance, restarts or crashes the server without an already provided control, reads host-level raw storage, or encounters any data that appears not to have been created for the investigation.

## Initial Run Order

For the first safe pass:

1. environment manifest;
2. CAT-001 through CAT-005;
3. create the minimal probe table and capture CTIDs;
4. QRY-001, QRY-002, QRY-004, and QRY-005;
5. PHY-001 and safe relation-size measurements;
6. MVCC-001 through MVCC-004 with two sessions;
7. VAC-001 on probe data;
8. PLN-001;
9. publish the first compatibility matrix and unanswered questions.

Do not begin crash, filesystem, replication, or destructive rewrite experiments during the first pass.

## Completion Criteria

The investigation is mature enough to guide Plumb implementation when:

- all major claims link to reproducible evidence;
- query semantics and MVCC behaviour have reference result sets;
- HOT, VACUUM, rewrite, and rollback cases are characterised;
- storage and segment hypotheses clearly state their confidence and alternatives;
- planner, count, and top-k behaviour are measured rather than inferred from marketing claims;
- durability tests cover committed and interrupted operations on disposable systems;
- the compatibility matrix distinguishes required API compatibility from optional implementation similarity;
- unresolved questions have prioritised next experiments.

## Source Material

- PlanetScale, [Introducing TIN](https://planetscale.com/blog/introducing-tin)
- PlanetScale, [Lead repository](https://github.com/planetscale/lead)
- PostgreSQL documentation, [Index Access Method Interface Definition](https://www.postgresql.org/docs/current/indexam.html)
- PostgreSQL documentation, [Index Access Method Functions](https://www.postgresql.org/docs/current/index-functions.html)
- PostgreSQL documentation, [Visibility Map](https://www.postgresql.org/docs/current/storage-vm.html)
- PostgreSQL documentation, [pageinspect](https://www.postgresql.org/docs/current/pageinspect.html)
- PostgreSQL documentation, [pg_visibility](https://www.postgresql.org/docs/current/pgvisibility.html)
