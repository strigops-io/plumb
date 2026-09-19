# PostgreSQL development-dataset baseline

`baseline.sql` measures the **current Plumb PostgreSQL extension** (`plumb` access
method and `~~>` operator) in this Plumb repository. It is not a benchmark of the
standalone in-memory postings crate, a production TIN instance, or evidence of a
performance improvement. Plumb's inherited Lead path emits all heap pages as lossy bitmap
candidates and rechecks the rows. Rare terms therefore need not yield fast scans,
and its index size does not represent a real postings index.

## Prerequisites and safety

- Use PostgreSQL 17 or 18 and a matching `psql` client, in a **disposable development
  database** with this checkout's `plumb` extension already built and installed.
  Follow the root README's public build/install instructions. An administrator may
  need to install/create the extension beforehand; this script never does so.
- No private regression repository, proprietary service, production TIN access,
  network dataset, or external data generator is needed.
- Start a fresh, dedicated `psql` session. The script owns a transaction, uses only
  newly created `TEMP` tables/indexes, schema-qualifies references with `pg_temp`,
  changes settings with `SET LOCAL`, and finishes with `ROLLBACK`. It never drops
  or modifies an existing user table, schema, extension, or index. Avoid sourcing
  it into a session with existing work or similarly named temporary objects.
- `ON_ERROR_STOP` makes SQL errors (including an ID mismatch) fail the invocation.
  A disconnected `psql -f` session rolls back on error too. If sourced manually
  into an interactive session and it fails, issue `ROLLBACK` or disconnect.
- Temporary does **not** mean cheap or memory-only: heap/index storage, local
  buffers, result materialization, and comparison spill files consume resources.
  Do not run on a production/shared server under load. Honor local disk quotas
  and consider a session `statement_timeout` / `temp_file_limit` appropriate to
  your environment (a temp-file limit does not cap the temporary table itself).

## Run from the repository root

Set `BENCH_DSN` to a local development connection string (prefer a service file or
`.pgpass`; do not put passwords in archived logs). `build_label` is operator-supplied
metadata: the database cannot prove which Git checkout produced an installed
binary. Record the **installed** Plumb revision and dirty state, not merely the
current checkout. The examples assume that the installed binary matches it.

```sh
export BENCH_DSN='dbname=lead_bench'
BUILD_LABEL="$(git rev-parse HEAD)" # append dirty/build flags if applicable

# Default: 100,000 rows, one measured pass for each query/path.
psql -X "$BENCH_DSN" -v build_label="$BUILD_LABEL" \
  -f benches/baseline.sql > baseline-100k.log 2>&1

# Opt in to larger scales only after checking time and disk at the smaller scale.
psql -X "$BENCH_DSN" -v rows=1000000 -v iterations=3 \
  -v build_label="$BUILD_LABEL" -f benches/baseline.sql \
  > baseline-1m.log 2>&1

psql -X "$BENCH_DSN" -v rows=10000000 -v iterations=3 \
  -v build_label="$BUILD_LABEL" -f benches/baseline.sql \
  > baseline-10m.log 2>&1
```

Check the command's exit status and final `baseline_complete_rolled_back` marker;
a partially written log is not a successful run. `rows` must be a positive
32-bit integer; `iterations` must be 1–100. Defaults are 100000 and 1. Values are
quoted as SQL literals before casting, not pasted into executable SQL. Use
`-v iterations=3` even at 100k for within-run repeat samples. More iterations
repeat measurements, **not** data generation/index build/correctness checking.

Ten million rows contain roughly 2 GB of text alone before tuple/page overhead,
auxiliary storage and result sets. The common query matches five million rows;
two bigint result tables and `EXCEPT ALL` operations require additional disk and
memory. `work_mem=64MB` is **per operation**, not a process/run memory cap. Expect
many complete scans, substantial generation/build time, and potentially long
runs. There is no guaranteed runtime or disk bound. Start with `-v rows=1000`
for a smoke test, then 100k, before considering 1m or 10m.

## Workload and correctness

The corpus has integer IDs 1 through `rows`, fixed ASCII filler, and these exact
term distributions (no randomness or external corpus):

| Case | TINQL query | Expected matching IDs |
| --- | --- | --- |
| Rare term | `rare` | `id % 997 = 0` |
| Common term | `common` | `id % 2 = 0` |
| AND | `rare AND common` | `id % 997 = 0 AND id % 2 = 0` |
| OR | `rare OR medium` | `id % 997 = 0 OR id % 101 = 0` |

At 100k rows these match 100, 50,000, 50, and 1,090 rows respectively. The common
term is not implied by the rare term, and the OR case has overlap at larger
scales. Expected counts are useful diagnostics, **not** the correctness test.

The index uses the public/tested syntax:

```sql
CREATE INDEX plumb_bench_docs_search ON pg_temp.plumb_bench_docs USING plumb (body);
```

No custom tokenizer/scoring reloptions are set. The default text operator and
explicit uppercase `AND`/`OR` match the implementation/tests in
`postgres/src/operator.rs`, `postgres/src/lib.rs`, and the defaults in
`postgres/src/options.rs`. Setup explicitly runs `ANALYZE`.

For every case, the script materializes all matching IDs under sequential-scan
and bitmap-scan planner settings. Two-way `EXCEPT ALL` compares the complete ID
multisets (including duplicate multiplicities), then compares the sequential
result to an independent arithmetic oracle. Any difference raises a SQL
exception. Only one case's pair of result tables is retained at a time. These
checks happen **after** the measured queries; their materialization/comparison
cost is not included in the JSON query execution times. No scoring or highlighting
is used: current heap-backed Plumb scoring can rescan and retokenize the corpus for every
score call and would distort or overwhelm this baseline.

## Measurements and interpretation

- `\timing` output reports setup/generation, index build, `ANALYZE`, and batch
  overhead separately. The DO measurement block's wall time includes all plans
  and bookkeeping; use each JSON plan's `Planning Time` and `Execution Time` for
  individual query observations. This is instrumented executor time, not a
  client fetching millions of results or an application's end-to-end latency.
- Each sample executes `EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON) SELECT id
  ...`. Labels include case, iteration, and **requested** path. `enable_seqscan`,
  `enable_bitmapscan`, `enable_indexscan`, and `enable_indexonlyscan` are set
  locally to request either a sequential or bitmap path. These flags discourage
  paths, not guarantee them. Inspect the actual node tree: expect `Seq Scan` or
  `Bitmap Heap Scan` with a `Bitmap Index Scan` using `plumb_bench_docs_search`.
  If these are not present, report that fact and do **not** label the observations
  a successful seq-vs-bitmap performance comparison. Correctness success alone
  does not establish path coverage.
- Inspect actual rows, rows removed by filter/recheck, lossy/exact heap blocks,
  buffer reads/hits and timing. Temporary relations use **local** buffers; sort
  and other spill I/O may appear as **temp** blocks. These are not shared-buffer
  workload measurements. Read counters do not prove physical disk I/O, since the
  operating system can serve reads from its cache. I/O timing appears only if
  enabled by the environment; the script does not request privileged changes.
- Serial execution and JIT-off are fixed for comparability; `work_mem` is fixed
  at 64 MB. Other settings are reported rather than overridden. Temp-table
  workloads cannot represent parallel scans of normal persistent tables.
- The log includes PostgreSQL version/build string, extension version/schema,
  supplied build label, settings, row/iteration inputs, index definition, and
  heap/table/index/total bytes. Extension version alone is not a source identity.
  Record separately: OS/kernel, CPU count/model, RAM, storage/filesystem, container
  limits, compiler/Rust version, build profile/features, pgrx version, installed
  source revision/dirty state, and concurrent load. Save the SQL alongside results.
- Logs interleave psql timing/phase text with compact one-line JSON records, rather
  than being one JSON document. Extract the records for analysis, for example:

  ```sh
  # Standard-library Python only; rejects malformed lines beginning with '{'.
  python3 -c 'import json,sys; [print(json.dumps(json.loads(s))) for s in sys.stdin if s.startswith("{")]' \
    < baseline-100k.log > baseline-100k.jsonl
  ```

## Reproducibility and cache state

Use the same installed build, server major, settings, machine, row count, and SQL
for before/after observations. Capture several independent runs; report all
samples or a median/range with the sample count, never only the best run. Analyze
setup costs separately from query costs. `ANALYZE` sampling and system scheduling
can still vary even though the corpus is deterministic. The paths alternate
order on successive iterations; query order remains fixed, so order bias remains.

For **warm** observations, request multiple iterations in the same session/data
set and compare later iterations. Manually note whether their local read/hit
patterns stabilize; a dataset larger than caches may never become fully warm.
Generation, index build, and `ANALYZE` already touch the corpus, so even iteration
1 is **not a cold-cache measurement**. Label it a first measured pass after setup.
A new invocation rebuilds and touches the data again; `DISCARD ALL` or a new
connection does not establish cold OS caches.

For a manual **cold-vs-warm investigation**, document actual cache conditions and
use a separately designed persistent-data experiment on a disposable host if
true cold reads are required; this temp-only baseline cannot survive a restart
with the same corpus. A host/server restart before this script is not sufficient
because setup warms the new corpus. Do not drop OS caches, flush shared production
buffers, or evict other users' working sets. This script intentionally supplies
no cache-reset commands and makes no cold-cache guarantee.

## Coverage limits

This is a synthetic, near-fixed-length, read-only, single-session ASCII workload with
no NULLs, deletes, updates, MVCC concurrency, phrases, fuzzy matching, languages,
custom tokenizers, joins, LIMIT/top-k, ranking, or sustained ingestion. Small heap-backed Plumb
index sizes reflect its all-page-candidate design, not compression efficiency.
Large development datasets may expose that design's linear scan/recheck costs;
measurements are not production capacity guidance, a comparison against TIN, or
evidence that the new in-memory code is integrated into PostgreSQL. No private
regression suite is needed to run or interpret this baseline.
