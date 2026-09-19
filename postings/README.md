# Scalar CTID postings

`plumb-postings` is Plumb's first storage-algorithm building block. It is an
in-memory, safe Rust library, not a PostgreSQL index, persistent segment format,
term dictionary or complete query executor. It has no third-party dependencies.

## Representation

- A `Ctid` stores a heap block number and one-based tuple offset. It rejects
  `u32::MAX` (invalid block) and offset zero. All other `u16` offsets are supported
  by the representation; the PostgreSQL adapter must enforce real page limits.
- CTIDs identify physical tuple versions, not stable logical document identities.
- `Postings` bulk-builds from arbitrary-order inputs, sorting and deduplicating.
- Aligned groups cover 256 heap blocks with four scalar `u64` page-mask words.
- Only present pages have offset vectors; their offsets are sorted and unique.
- Iteration is in ascending CTID order. Empty pages/groups are never retained.
- Union and intersection combine page masks before merging offsets. Difference
  retains shared pages until offset subtraction—subtracting page masks would
  incorrectly remove other tuples on those pages.

All fields are private. Public constructors and immutable set operations preserve
these invariants. There is no surrogate document-ID mapping and no unsafe code.

```rust
use plumb_postings::{Ctid, Postings};
let a = Postings::from_ctids([Ctid::new(255, 1)?, Ctid::new(256, 2)?]);
let b = Postings::from_ctids([Ctid::new(256, 2)?]);
assert_eq!(a.intersection(&b).len(), 1);
# Ok::<(), plumb_postings::CtidError>(())
```

`difference` is finite set subtraction, **not** a TINQL/SQL NOT implementation.
Compiling NOT requires a defined indexed-document universe, NULL semantics and
visibility handling. The library does not decide visibility, HOT root identity,
transaction status, CTID reuse or whether a tuple may be reclaimed.

## Resource boundary

Bulk build collects all input, sorts in O(n log n), then constructs sparse groups.
It is not an external-memory build. Set operations allocate complete output sets;
matching page lookup uses binary search within each group (at most 256 pages),
then merges sorted offsets. Vectors use the Rust allocator, not PostgreSQL memory
contexts. Input and output can coexist in memory, and allocation is not bounded by
`work_mem` or `maintenance_work_mem`. Dense offset codecs, compression, streaming,
interrupt checks and PostgreSQL memory accounting remain future work.

Do not wire this directly into the access method without resolving those resource
and correctness boundaries. There is deliberately no serialization API or promise
of a stable on-disk layout yet.

## Validation

From the repository root:

```sh
cargo test --locked -p plumb-postings
cargo test --locked --release -p plumb-postings
cargo clippy --locked -p plumb-postings --all-targets -- -D warnings
```

Tests cover every page-mask bit, 63/64 and 255/256 boundaries, high block numbers,
all nonzero u16 offsets, duplicate/empty input, representation invariants,
96 deterministic generated corpus pairs and all 4,096 pairs of subsets of a small
CTID universe. Operations are compared with `BTreeSet` references and algebraic
identities. These tests establish set behavior, not PostgreSQL MVCC or durability.

AGPL-3.0-or-later; see the repository's LICENSE.
