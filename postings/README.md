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
and correctness boundaries. The standalone codec below does not establish a stable
PostgreSQL on-disk layout or production upgrade path.

## Experimental v1 codec

`plumb_postings::codec::{encode, decode, DecodeLimits, CodecError}` serializes one
finite CTID set in a documented little-endian frame, with a versioned 40-byte
header and CRC32C covering metadata and body. See the exact
[format contract](../docs/postings-format.md).

Decode validates the complete frame without allocation, then constructs grouped
postings using fallible exact reservations. It rejects unsupported versions,
unknown flags, corruption, noncanonical groups/offsets, truncation, trailing bytes,
count mismatches and caller-limit violations. Limits cover encoded bytes, groups,
pages, postings and requested decoded vector element storage. The storage budget
is **not a process RSS cap** and excludes allocator overhead and borrowed input.
The encoder accepts a validated set and allocates its complete output, with no
separate caller budget.

This crate itself does not implement dictionaries, metapages, PostgreSQL buffers
or WAL. The experimental adapter in postgres/src now embeds the codec in immutable
term segments and can prune positive SQL queries; its separate constraints and
validation are documented in docs/checkpoint-003-results.md. The crate remains dependency-free and forbids
unsafe code; the separate allocation-failure test binary narrowly uses a documented
unsafe allocator wrapper to exercise errors, not in library code.

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
identities. Codec tests additionally cover pinned golden bytes, corrupt and resealed
frames, bounded generated inputs, limits, allocation failure at every reservation,
and allocation-free rejection. All 32 postings tests/doctests pass in debug and
release builds. These establish set/codec behavior, not PostgreSQL MVCC or durability.

AGPL-3.0-or-later; see the repository's LICENSE.
