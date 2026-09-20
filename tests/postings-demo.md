# Persistent postings: disposable SQL demonstration and validation

**Status: executed on PostgreSQL 17.10; checkpoint 004 now uses the persisted engine by default.**
The SELECT demo, mutation matrix, concurrent writers, actual immediate-stop recovery,
pre-build HOT and proven partial-index CTID reuse passed. See the
[recorded results](../docs/checkpoint-003-results.md) and
[storage contract](../docs/persistent-index-milestone.md). These are local evidence,
not a production safety certification.

## Safety and prerequisites

- Use PostgreSQL 17 and a **new, isolated, disposable WAL-enabled cluster**, an
  explicit socket/port/user, and a fresh database named `plumb_postings_*` created
  from `template0`. Install this checkpoint's non-`pg_test` Plumb build first using
  the packaging guidance in [`README.md`](README.md). Do not pre-create the extension.
- Unlike the coexistence script, **these scripts COMMIT permanent data and indexes**.
  This is deliberate: another connection must be able to verify them after an
  immediate-stop restart. An error after the first commit leaves fixtures behind.
- Both scripts require `-v disposable_db=EXACT_DATABASE_NAME`; the name must match
  the current connection and prefix. The demo refuses existing user relations,
  nonstandard schemas, public functions, or extensions other than `plpgsql`.
  It never uses `IF NOT EXISTS`, `CREATE OR REPLACE`, `DROP`, or `CASCADE` to adopt
  or overwrite existing objects. These guards are accident prevention, not a
  security boundary against an adversarial database owner.
- Initial creation uses `plumb_postings_demo`; mutations require its marker and
  database OID and create a **new** `plumb_postings_mutations` schema. Mutations
  never update, delete, truncate, or reindex the demo's original `docs`/index.
  They append diagnostic rows to the demo's evidence table. Rerunning mutations
  refuses an existing mutation schema; start another fresh database instead.
- Run using `psql -X`, not `--single-transaction`: explicit commits and the
  out-of-transaction `VACUUM` are essential. `ON_ERROR_STOP` is set in the scripts.
  Use a dedicated extension-installing role; no other session should write fixtures.
- `fsync` and `full_page_writes` must be on. The demo asserts both, sets
  `synchronous_commit=on`, disables parallel query/JIT for readable plan assertions,
  and gives the bitmap 64 MiB `work_mem`. The fixtures remain intentionally small
  in vocabulary and posting count; they do not test resource exhaustion.

The scripts do not create/drop databases, change server configuration, or control
services. Review every path/connection before executing the operator-run examples.

## Run order and saved artifacts

After separately provisioning/installing the disposable cluster:

```sh
# Replace these with the socket/port/user of YOUR NEW disposable cluster.
export PGHOST=/absolute/path/to/disposable-socket PGPORT=55439 PGUSER=your_test_role
createdb -T template0 plumb_postings_demo01
mkdir -p /absolute/path/to/postings-results

psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_demo01 \
  -d plumb_postings_demo01 -f tests/postings_demo.sql \
  > /absolute/path/to/postings-results/demo.log 2>&1
# Check exit status: do not continue on failure.

# Reopen a new connection: verify without CREATE EXTENSION/reload/REINDEX.
psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_demo01 -v verify_only=on \
  -d plumb_postings_demo01 -f tests/postings_demo.sql \
  > /absolute/path/to/postings-results/reopen.log 2>&1

psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_demo01 \
  -d plumb_postings_demo01 -f tests/postings_mutations.sql \
  > /absolute/path/to/postings-results/mutations.log 2>&1

# Machine-readable JSON Lines, including natural/forced plans and rejection details.
psql -X -v ON_ERROR_STOP=1 -At -d plumb_postings_demo01 \
  -c 'SELECT row_to_json(e) FROM plumb_postings_demo.evidence e ORDER BY evidence_id' \
  > /absolute/path/to/postings-results/before-restart.jsonl
```

Do not concatenate these steps blindly: check each exit code/log. Output filenames
are examples and shell redirection overwrites them. Pick new filenames per run.
`plumb_postings_demo.evidence` is also a permanent in-database record. The scripts
print key JSON and readable sequential/index/oracle counts; all forced/reference
JSON plans remain available in the evidence table. Validation-stage failures roll
back that stage's evidence; retain stderr and inspect the already committed fixture.

## Corpus, expected results, and exact pruning gate

The permanent heap has IDs 1–30,000, approximately **1,063 bytes of body text per
row**, plus a NULL document (30,001) and an empty document (30,002). `STORAGE PLAIN`
prevents compression from invalidating the physical-size premise. Four repeated
words plus parity/rare/two marker words keep vocabulary tiny. The index is built
**after** loading all 30,002 rows, yielding multiple bounded bulk segments in checkpoint 004.

`beacon` appears exactly when `id % 997 = 0` among the 30,000 regular documents:

- **30 hits**: 997, 1,994, …, 29,910.
- `beacon AND copper`: **15 hits** (even multiples of 997).
- `beacon OR leftmark`: **31 hits** (rare IDs plus ID 1).
- `leftmark AND rightmark`: **0**; `leftmark OR rightmark`: **2** (IDs 1, 2).
- `absentmarker`: **0**.

The script checks actual heap blocks exceed 256, rare IDs span more than one
256-block CTID group, and marker IDs 1/2 occupy the same page at distinct tuple
offsets. No planner estimate substitutes for these physical checks.

For every query, independently planned dynamic SQL collects full IDs under a
verified sequential scan and a forced verified bitmap index scan. Bidirectional
`EXCEPT ALL` comparisons catch both missing/extra results and multiplicity
mismatches, not just equal counts. Positive demo cases also compare the full result
to independent arithmetic ID generators. NULL/empty rows participate throughout.
The SQL binds `OPERATOR(pg_catalog.~~>)` explicitly and schema-qualifies Plumb
functions and permanent fixtures.

Both natural and forced `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` are saved. The
natural planner is free to choose a sequential scan; there is no latency claim.
The forced rare-term plan must contain the expected `docs_postings` Bitmap Index
Scan and a Bitmap Heap Scan with:

- exactly **30 actual output rows**;
- positive `Exact Heap Blocks`, **strictly less than total heap blocks / 10**;
- `Lossy Heap Blocks = 0`.

Phrase (`"amber cedar"`), NOT (`amber AND NOT beacon`), and wildcard (`bea*`) are
checked against full sequential results, including counts. **No exact/pruning
requirement** is imposed on these fallback queries. Their expected semantics come
from the inherited operator, not a guessed phrase/wildcard implementation.

## Mutation gates

The separate permanent mutation fixture has 600 regular rows, NULL/empty rows,
`fillfactor=50`, and autovacuum disabled only on that disposable table. Nine query
forms are compared after each of: initial build, insert, savepoint-aborted inserts,
indexed-body updates (including NULL/empty), nonindexed `version` updates, delete,
explicit VACUUM, post-VACUUM reinserts, reloption toggle plus writes, REINDEX,
TRUNCATE, and post-TRUNCATE inserts. The simple `beacon` result also uses a
non-operator `strpos` oracle over the controlled corpus after every stage.

- Nonindexed updates attempt HOT with reserved page space; old/new CTIDs are
  recorded. Same-page movement is **not proof of HOT**. CTID reuse after VACUUM is
  also an attempt, not an assertion about PostgreSQL's allocator. Check table
  statistics independently if positive evidence of HOT is required.
- Changing a built postings index's reloption to `heap` must preserve scans and
  subsequent insert/update/delete maintenance. Its persisted `format_version`
  must remain 1. The option is restored to `postings_v1` before REINDEX.
- Flipping a populated heap-baseline index to `postings_v1` must fail at ALTER,
  or both forced scan and insert must reject the incompatible state. **Successful
  but silently empty/stale reads or writes fail the test.** The baseline is
  restored to `heap`, its result compared to ID 1, and row count checked for
  atomic rejection of the attempted insert.
- REINDEX/TRUNCATE may succeed correctly or explicitly reject under the milestone
  contract. Only SQLSTATE `0A000` plus an unsupported-operation message is accepted
  as a safe rejection; other errors fail. Results are revalidated in either case.
  Evidence distinguishes “succeeded” from “explicitly rejected”; a rejection is
  not reported as supported maintenance. If the implementation uses another
  deliberate SQLSTATE, review an observed failure before adapting this expectation.
- Valid nondefault `tokenizer='whitespace'`, temporary heap, and unlogged heap
  postings builds must reject. Negative fixtures are not the demonstration heap.
  Error messages and SQLSTATEs are saved; unexpected message categories fail.

### Manual concurrent-build rejection (outside a transaction)

Use **only after** the mutation script succeeds. This must not be hidden in a DO
block/function/transaction: PostgreSQL would reject the transaction context before
Plumb's concurrency check. Review the server/DB identity first, then in a new
`psql -X` session with `ON_ERROR_STOP` and the same `disposable_db` guard:

```sh
psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_demo01 -d plumb_postings_demo01 <<'SQL'
SELECT current_database() = :'disposable_db'
  AND current_database() LIKE 'plumb\_postings\_%' ESCAPE '\' AS safe_database \gset
\if :safe_database
\else
  \quit 3
\endif
SELECT plumb_postings_demo.check((SELECT count(*)=1 FROM plumb_postings_demo.identity
  WHERE marker='postings-demo-v1' AND database_name=current_database()
    AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())), 'manual concurrency identity');
CREATE INDEX CONCURRENTLY rejected_concurrent
  ON plumb_postings_mutations.baseline USING plumb(body) WITH(storage='postings_v1');
SQL
```

Expected: a **nonzero exit and a Plumb-specific concurrency rejection**, not
“cannot run inside a transaction block.” A successful build fails this negative
gate. A failed concurrent build may leave an invalid `rejected_concurrent` index;
record catalog state (`pg_index.indisvalid`, `indisready`) before disposing of the
database. Do not retry with the same name or silently clean it up. Save stderr.

## Immediate-stop restart evidence (operator-controlled)

After a successful demo (and optionally mutations), export evidence and disconnect
all test clients. **Only an operator explicitly authorized to stop this exact new
disposable cluster should perform this step.** Do not stop an existing shared or
production service. No server-control operation is performed by the SQL scripts.

1. Verify the cluster data directory/socket/port and settings; retain server logs.
   Stop that dedicated cluster with `pg_ctl -D /absolute/path/to/disposable-pgdata
   stop -m immediate`. Restart the **same** data directory with its original
   socket/port configuration. Do not reinitialize, recreate extension/index,
   reload data, REINDEX, or use a clean shutdown as a substitute. Avoid an explicit
   checkpoint immediately before the stop; the aim is to exercise crash recovery.
2. Confirm startup log records recovery from the immediate stop. Run the demo
   again with **`-v verify_only=on`**, saving `after-restart.log`. It reuses the
   original corpus/index, rechecks full IDs and exact bitmap bounds, checks metadata
   and appends evidence; it does not rebuild or mutate the corpus.
3. If mutations were run, reconnect and run
   `SELECT plumb_postings_mutations.validate('after immediate-stop recovery');`
   with the same isolated connection and `work_mem='64MB'`, parallel query/JIT off.
   Export the entire evidence table to a new `after-restart.jsonl` file.
4. Compare before/after `index_stats` for the untouched original index. Expected:
   identical `format_version`, `segments`, `payload_bytes`, `relation_blocks`, and
   identical arithmetic ID results. Query timing/buffer-cache figures may differ.
   Capture the precise build revision, PostgreSQL version, settings, and startup
   recovery log alongside JSON; a `PASS` without these is not a recovery report.

This is local recovery **evidence, not proof** of PITR, streaming replication,
corruption tolerance, failover behavior, or production readiness. A test may not
force every WAL record to be replayed; report what the startup log actually says.
Dispose of the database/cluster only after deliberately rechecking the connection
and preserving artifacts. No automatic destructive cleanup is supplied.

## API and JSON assumptions / explicit limits

The scripts exercise this implemented API:

```sql
CREATE EXTENSION plumb;
CREATE INDEX example ON some_permanent_table USING plumb(body) WITH(storage='postings_v1');
SELECT plumb.index_stats('schema.index_name'::regclass);
```

`index_stats` is castable to `jsonb` and returns a **flat object**, with integer-like
`format_version`, `segments`, `payload_bytes`, `relation_blocks` keys. Version 1 and
multiple bounded bulk segments are asserted for the untouched demo. Physical block count equals
MAIN-fork `pg_relation_size / current_setting('block_size')`. Missing keys fail
rather than becoming a silent default. PostgreSQL EXPLAIN is the normal one-element
JSON array; plan children are under `Plans`, and fields include `Node Type`,
`Index Name`, `Relation Name`, `Actual Rows`, `Exact Heap Blocks`, `Lossy Heap Blocks`.
These fields and plan shapes were verified in the recorded PG17 run.

Additional scripts:

```sh
python3 tests/postings_concurrency.py --host "$PGHOST" --port "$PGPORT" \
  --database plumb_postings_demo01
psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_demo01 \
  -d plumb_postings_demo01 -f tests/postings_edges.sql
# After the authorized crash/restart and demo verify_only/mutation checks:
psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_demo01 \
  -d plumb_postings_demo01 -f tests/postings_recovery.sql
```

The concurrency runner uses only Python's standard library and psql, explicit local
Unix sockets, advisory gates and anchored snapshots. It refuses existing fixtures.
The edge script proves a HOT update before build and actual CTID reuse outside a
partial predicate. The recovery script checks the committed concurrency fixtures
without rebuilding indexes. The SQL pgrx suite separately covers expressions,
partial predicates, multiple keys, rescans, analyzer changes and opclass validation.

This focused suite does not prove cap-exhaustion behavior, crash/cancellation at
every publication boundary, long-run stress, replication/PITR, all heap rewrite
paths, or hosted proprietary TIN compatibility. It enforces correctness/pruning,
not a general throughput or latency guarantee.

## Checkpoint 004 growth and merge

After this demo succeeds in a fresh local database named `plumb_postings_checkpoint004`:

```sh
psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_postings_checkpoint004 \
  -d plumb_postings_checkpoint004 -f tests/default_engine_growth.sql
python3 -B tests/merge_concurrency.py --host "$PGHOST" --port "$PGPORT" \
  --database plumb_postings_checkpoint004
```

These create new guarded schemas; never rerun over existing fixtures. Growth checks
200k rows and 1,000,201 term/CTID pairs with **no storage reloption**, compares rare,
AND/OR results, and verifies a too-large merge fails without metadata/size changes.
The merge runner checks existing old-visible versions, active committing/aborting
appenders, competing mergers, and deterministic TRUNCATE lock order. It leaves a
manifest for `--verify-only` after a separately authorized disposable-cluster crash/
restart. Use new `--log`/`--summary` output paths for recovery results. See
[checkpoint-004-results.md](../docs/checkpoint-004-results.md) for recorded evidence.
