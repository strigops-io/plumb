# Plumb

> [!WARNING]
> **A note from Plumb's maintainers**
>
> We wouldn't run this for anything important, and neither should you.  We're not familiar with Postgres internals, Rust, or implementing full text search.  This is strictly a proof of concept to see how much we can improve performance using exclusively AI Agents.  It's, at best, interesting.

Plumb is an independent fork of [PlanetScale Lead](https://github.com/planetscale/lead),
modified since 2026-09-20. Licensed under **AGPL-3.0-or-later**.
Plumb is not affiliated with, endorsed by, or supported by PlanetScale.
PlanetScale, Lead, and TIN are names used by PlanetScale.
**Do not report Plumb bugs to PlanetScale.** Use [Plumb issues](https://github.com/strigops-io/plumb/issues).

Plumb installs as `CREATE EXTENSION plumb`, exposes its own `plumb` access method,
`plumb` schema and scoring functions, and uses **`~~>`** as its search operator.
Existing Lead/TIN `tin` names and `==>` are left untouched.

```sql
CREATE EXTENSION plumb;
CREATE TABLE documents (id bigint PRIMARY KEY, body text);
-- Experimental persisted postings; omit WITH to keep the old heap baseline.
CREATE INDEX documents_plumb_idx ON documents USING plumb (body)
WITH (storage = 'postings_v1');
SELECT id, plumb.full_score(ctid)
FROM documents WHERE body ~~> 'search';
```

An optional TIN-name compatibility extension is a **later-phase goal, not shipped**.
Do not run `CREATE EXTENSION tin` expecting Plumb aliases: today it installs the
separately supplied Lead/TIN provider, if present. See the
[coexistence design](docs/coexistence.md) for same-database use and compatibility boundaries.
Original copyright and warranty notices are retained; see [LICENSE](LICENSE),
[NOTICE](NOTICE) and the [source-distribution/network-use checklist](docs/license-compliance.md).

**Proof of concept / investigation only. Not production-ready.**

[Plumb](https://github.com/strigops-io/plumb) is a fork of [PlanetScale Lead](https://github.com/planetscale/lead), investigating a more production-oriented implementation of its PostgreSQL text-search compatibility layer. The immediate goal is a coherent local developer experience for applications targeting PlanetScale TIN, without needing to provision a real PlanetScale instance for ordinary development and tests.

This investigation asks two questions:

1. **Can this run relatively performantly on large development datasets?** Measure build time, query latency, memory and storage costs rather than assume the answer.
2. **How much engineering would a credible escape hatch from PlanetScale's hosted search require?** Assess compatibility, correctness, durability and operations needed to reduce vendor lock-in—not just whether the SQL parses.

“Production-oriented” describes the direction, not the current capability or a support promise. This is not a replacement for PlanetScale's entire platform, nor a proven self-hosted TIN equivalent. See [VISION.md](VISION.md) for the long-term architecture and [the implementation status](docs/implementation-status.md) for what actually exists.

## Current milestone

- Retains Lead's TINQL, tokenizers, scoring/highlighting behavior and heap-backed access method, with independent Plumb SQL identities. Inherited behavior assertions are retained with name/operator substitutions; new tests enforce provider isolation.
- Uses the zero-dependency [CTID postings core](postings/) and [v1 codec](docs/postings-format.md) in an **opt-in persistent SQL index**. Term frames retain 256-page groups, page-presence masks and sparse tuple offsets, with no surrogate document-ID mapping.
- Adds a versioned metapage and immutable linked term segments in PostgreSQL-managed MAIN-fork pages. Generic WAL writes segment pages before publishing the new head. Inserts append segments; exact candidates always undergo heap visibility and operator rechecks.
- Adds a [reproducible PostgreSQL baseline](benches/README.md) with generated datasets, query plans and full result-set checks; no hosted service or proprietary test suite is required.

**The default remains the heap-backed compatibility baseline.** Explicit `WITH (storage = 'postings_v1')` enables the experimental stored-postings path for positive terms and AND/OR queries. Unsupported query shapes (including phrases, NOT and wildcard forms) fall back to all-heap-page rechecks. Scoring is still heap-backed.

[Checkpoint 003 demonstrates a real database SELECT](docs/checkpoint-003-results.md): 30 exact matching heap pages out of 4,286, with a nonzero persistent index. This is a small synthetic proof, not a general performance or production-readiness claim. Run the [guarded SQL demonstration](tests/postings-demo.md) to reproduce it.

The extension/package/library, schema and access method are `plumb`, version `0.1.0`. The standalone postings crate also starts at `0.1.0`; untouched language crates retain their upstream versions. This is not an in-place upgrade of an existing Lead/TIN index: create a separate Plumb index and deliberately migrate queries.

## Build and local development

Use the pinned Rust toolchain in `rust-toolchain.toml`. [Install `cargo-pgrx`](https://github.com/pgcentralfoundation/pgrx/blob/develop/cargo-pgrx/README.md) version 0.19.1 exactly and initialize it for the PostgreSQL major version you need:

```sh
cargo install cargo-pgrx --version 0.19.1 --locked
cargo pgrx init --pg18=download
cargo pgrx package --package plumb --no-default-features --features pg18
```

For an interactive local development database:

```sh
cargo pgrx run pg18 --package plumb --no-default-features --features pg18
```

Then run `CREATE EXTENSION plumb` in that database. For PostgreSQL 17, use `pg17` consistently instead. The extension loads on demand; it does not require `shared_preload_libraries` or `session_preload_libraries`.

## Compatibility boundary

The Plumb API provides the `plumb` access method, `~~>` operator, TINQL parsing, tokenizer and index reloptions, scoring functions `plumb.score`, `plumb.full_score`, `plumb.max_score`, and `plumb.score_inspect`, plus explicit and implicitly bound `plumb.highlight` and `plumb.highlight_ansi`. Their underlying behavior is inherited from Lead; this is query-language/behavior compatibility, not drop-in SQL-name compatibility. PostgreSQL 17 and 18 are build targets. Search results are rechecked against visible heap tuples, including expression and partial-index rechecks.

Scoring deliberately rescans and retokenizes the visible indexed column or expression. A score call must be in the same query level as the matching Plumb `~~>` predicate. Implicit highlighting has the same binding boundary; passing its `query` argument explicitly works without a bound predicate. No additional hosted TIN equivalence is claimed by this milestone.

## Execution and storage today

- `storage='heap'` (default): original all-heap-page lossy recheck implementation, with no stored search postings.
- `storage='postings_v1'` (opt-in): bulk-builds one immutable term segment; inserts append segments. Positive queries union each term across segments, combine grouped CTID sets and return exact bitmap candidates with mandatory heap recheck. No extension files live outside PostgreSQL relation storage.
- A persisted metapage remains authoritative even if the reloption is toggled to `heap`. Converting a zero-page heap-baseline index to postings requires REINDEX; toggling a setting never silently skips maintenance.
- Deletes/aborted writes can leave stale positive postings. MVCC/rechecks filter them; VACUUM does **not** reclaim index postings yet. Tested REINDEX compacts by rebuilding; tested TRUNCATE rebuilds an empty index.

### Hard limits and unsupported operations

Postings mode currently accepts permanent heaps, the canonical text-search operator semantics, deterministic collation and the default analyzer only. Concurrent index build/reindex, temporary/unlogged storage and incompatible opclasses are rejected. Current limits include a 16 MiB document/segment cap, 262,144 build term/CTID pairs, 64 MiB committed index payload, 65,536 segments and 256 MiB physical index size. Queries have separate text/node/candidate limits and may error at a cap. Limits are not `work_mem`/RSS accounting; allocator overhead and transient buffers are additional.

Every insert currently creates a small immutable segment; every postings query traverses committed segments. There is no mutable ingestion buffer, background merge, dead-posting reclamation, index-backed scoring, top-k, or robust large-dataset tuning yet. A local immediate-stop/WAL-redo test passed, but replication, PITR, cancellation at publication boundaries and long stress testing remain unproven. See [storage contract](docs/persistent-index-milestone.md) and [measured evidence/limitations](docs/checkpoint-003-results.md).

These 0.1.0 checkpoints target fresh disposable databases. No extension upgrade scripts or production on-disk migration path are supplied; installing new files over an earlier loaded development version is not a tested upgrade procedure.

## Tests and measurements

Run the inherited behavior tests and new identity/isolation tests:

```sh
cargo pgrx test pg18 --package plumb --no-default-features --features pg18
```

The pure Rust suites, including the new postings core, run without PostgreSQL or pgrx initialization:

```sh
cargo test --locked -p plumb-postings -p tinql -p tokenizer -p boldi-vigna
cargo clippy --locked -p plumb-postings --all-targets -- -D warnings
```

The [persistent SELECT/mutation/recovery tests](tests/postings-demo.md) exercise the opt-in stored index. The [coexistence tests](tests/README.md) exercise Plumb alongside separately installed public Lead. The [baseline guide](benches/README.md) explains opt-in 100k, 1m and 10m-row SQL measurements and their resource limits. Scale up only on a disposable development database. The current implementation makes no large-dataset performance promise.

The upstream private-regression helper scripts are retained unchanged for provenance; they still target upstream `tin` and are not supported Plumb commands or part of CI. This investigation uses public source/documentation and, only when separately authorized and supplied, black-box SQL observations—not proprietary TIN source or extracted binaries.

## TINQL guide

The [TINQL guide](tinql/docs/src/SUMMARY.md) documents the query language. To build it with mdBook, run from the repository root:

```sh
cargo install mdbook --version 0.5.2 --locked
mdbook build tinql/docs
```

Open `tinql/docs/book/index.html` in your browser to read the book.

## Contributing

Report Plumb issues in [strigops-io/plumb](https://github.com/strigops-io/plumb/issues), not through PlanetScale support. Include a minimal SQL reproduction, expected and actual results, PostgreSQL version, source revision, and relevant plans. Label evidence as inherited behavior, public documentation, authorized observation, or a Plumb-specific difference. Compatibility is measured, not assumed.

## Attribution and license

Derived from PlanetScale Lead. Existing PlanetScale copyright notices and the [AGPL-3.0-or-later license](LICENSE) are preserved. New Plumb code uses the same license. Plumb is an independent investigation; it is not PlanetScale's private TIN implementation and is not an official PlanetScale product.
