# Plumb identity and coexistence tests

## PostgreSQL integration tests

From the repository root, with Rust, cargo-pgrx 0.19.1 and PostgreSQL 17 configured:

```sh
cargo fmt --package plumb -- --check
cargo pgrx test pg17 --package plumb --no-default-features --features pg17 \
  --pgdata /absolute/path/to/new-disposable-pgdata
cargo clippy --package plumb --all-targets --no-default-features \
  --features pg17,pg_test -- -D warnings
```

The inherited assertions remain intact (SQL identity/operator substitutions only).
`postgres/src/identity_tests.rs` adds checks for canonical catalog identity and
extension ownership, absence of accidental `tin` installation/aliases, exact
operator-OID matching across namespace and argument-signature shadows, rejection
of foreign implicit scoring/highlighting, and fresh lookups after catalog changes.

`cargo pgrx test` installs the Plumb test build into the configured PostgreSQL
installation. Use a dedicated development installation, never a production
server. Package and install a non-`pg_test` build before the coexistence test:

```sh
cargo pgrx package --package plumb --pg-config /path/to/pg_config \
  --no-default-features --features pg17 --out-dir /absolute/path/to/plumb-stage
```

Only install the staged **plumb** library, control and SQL files into that
PostgreSQL installation. Never overwrite or remove Lead's `tin` files.

## Public Lead coexistence

**DESTRUCTIVE TEST — disposable, empty database only. Review
[`coexistence.sql`](coexistence.sql) before execution.** It deliberately issues
`DROP EXTENSION ... CASCADE` for each provider. Never run it against application
or benchmark data. Although all DDL is enclosed in a transaction and rolled back,
that is not a substitute for using a fresh disposable database/cluster.

Install **unchanged public PlanetScale Lead** separately as extension `tin`
(AM/schema `tin`, operator `pg_catalog.==>`), and Plumb as extension `plumb`
(AM/schema `plumb`, operator `pg_catalog.~~>`), using the same PostgreSQL major
version. This test does **not** represent testing of hosted/proprietary TIN.
Plumb ships neither `tin.control` nor SQL compatibility aliases.

Start a new isolated PostgreSQL cluster with its own data directory and socket.
Use explicit connection parameters to avoid accidentally reaching another server.
For example, after initializing and starting that disposable cluster:

```sh
export PGHOST=/absolute/path/to/disposable-socket PGPORT=55439
createdb -T template0 plumb_coexist_identity
psql -X -v ON_ERROR_STOP=1 -v disposable_db=plumb_coexist_identity \
  -d plumb_coexist_identity -f tests/coexistence.sql
# Only after confirming PGHOST/PGPORT still identify your disposable cluster:
dropdb plumb_coexist_identity
```

The script refuses mismatched names, names without the `plumb_coexist_` prefix,
preexisting user relations, provider schemas, or extensions other than `plpgsql`.
It creates both extensions itself; do not pre-create them in the test database.
A successful run prints a check count and `PASS`, then leaves the database empty.
An error exits psql and rolls the transaction back.

Coverage:

- Installation fails transactionally on preexisting handler/operator collisions,
  preserving the original objects rather than replacing or adopting them.

- Independent extension, schema, operator, procedure/library, opclass and AM
  identities and extension membership; Lead procedure OIDs/definitions unchanged.
- Both AMs indexing the **same table**; recursive `EXPLAIN (ANALYZE, FORMAT JSON)`
  assertions verify the expected index, whose catalog AM is checked separately.
- Matching rows, positive/ranked scores, exact highlights, and mixed-provider
  score/full-score/max-score/HTML/ANSI results equal separate baselines.
- Negative operator/provider cross-binding, foreign index inspection, and
  rejection when only the other provider's index exists.
- Same-spelling operator shadowing via explicit `search_path` and shadowed
  internal `score_bound`; qualified Plumb binding remains isolated.
- Both installation orders and both drop orders: the surviving provider keeps
  its index, operator, scoring, highlighting and index scan behavior.

These are correctness/identity checks, not a performance claim. The default heap
baseline coexists with Lead; [postings-demo.md](postings-demo.md) exercises the new
opt-in persistent postings path. Neither is production-ready.
