# Experimental scalar postings codec, version 1

This is a **standalone codec for one finite CTID set**. It is not yet a PostgreSQL
relation format, metapage, term dictionary, segment directory, WAL record or
committed persistent index. There is no SQL integration or production-format
upgrade promise. Format versioning is independent from extension version 0.1.0.

The purpose is a small, explicit wire format and hostile-input validation boundary
before introducing PostgreSQL-managed storage. The existing 256-page grouping is
preserved rather than flattening all CTIDs or inventing logical document IDs.

## Public API contract

In `plumb_postings::codec`:

```rust,ignore
pub fn encode(postings: &Postings) -> Result<Vec<u8>, CodecError>;
pub fn decode(bytes: &[u8], limits: DecodeLimits) -> Result<Postings, CodecError>;

pub struct DecodeLimits {
    pub max_encoded_bytes: usize,
    pub max_postings: usize,
    pub max_groups: usize,
    pub max_pages: usize,
    pub max_decoded_bytes: usize,
}
```

`DecodeLimits` implements `Default`, `Clone`, `Copy`, and `Debug`. The defaults are
64 MiB encoded bytes, 8,000,000 postings, 65,536 groups, 1,000,000 pages, and
128 MiB decoded structural storage. Limits are inclusive. The caller may lower
limits to zero, including to prohibit nonempty input. Zero structural-storage
budget permits the empty set (the stack `Postings` object is not charged).
`CodecError` implements `Debug`, `Display`, and `std::error::Error`. Malformed,
unsupported, over-limit or allocation-failing inputs return errors, not a panic.

## Byte order and frame layout

All integers are unsigned, **little-endian**, with no implicit/native alignment or
padding. A frame is a 40-byte header followed by exactly `body_bytes` bytes.

| Header offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 8 | Magic bytes: ASCII `PLMBPST` followed by NUL (50 4c 4d 42 50 53 54 00) |
| 8 | 2 | Format version = 1 |
| 10 | 2 | Flags = 0; reject any unknown flag |
| 12 | 4 | Header length = 40; reject any other value |
| 16 | 4 | Group count |
| 20 | 4 | Reserved = 0 |
| 24 | 8 | Exact total posting count |
| 32 | 4 | Exact body length in bytes |
| 36 | 4 | CRC32C of header bytes 0..36 followed by the complete body |

The four checksum bytes themselves are omitted from the checksum calculation.
CRC32C uses the reflected Castagnoli polynomial `0x82f63b78`, initial register
`0xffffffff`, reflected byte processing and final XOR `0xffffffff`. The standard
check value for ASCII `123456789` is `0xe3069283`. It detects accidental corruption;
it does not authenticate data or defend against a deliberately recomputed checksum.

The body repeats `group_count` records in strictly increasing heap-block-base
order. Each group consists of:

1. aligned heap block base: `u32`, divisible by 256;
2. page-presence mask: four `u64` words; word 0 describes relative pages 0..63,
   word 1 pages 64..127, etc.; bit 0 is the lowest page in each word;
3. for every set bit, in increasing relative-page order: a `u16` offset count,
   followed by that many `u16` tuple offsets in strictly increasing order.

Group masks must be nonzero. Offset counts must be 1..65535 and offsets must be
nonzero, unique and ordered. Each reconstructed block must be valid (not
`u32::MAX`). The largest aligned base is valid only when its mask excludes page
255. The sum of all offset counts must equal the header's posting count. Groups
with zero pages, duplicate/descending groups, duplicate offsets or empty pages are
not canonical and must be rejected rather than silently sorted or deduplicated.

An empty set has zero groups/postings/body bytes and still includes a valid header
and checksum. Nonempty frames require nonzero groups and postings. Appended bytes,
truncation, length/count disagreements and nonzero reserved fields are errors.
The encoder emits one canonical representation for a given CTID set.

## Decoder resource and safety contract

- Reject encoded length over the caller's limit before checksum or body work.
- Validate lengths using checked arithmetic and checked integer conversions; never
  cast untrusted counts into an allocation request without verification.
- Verify header/version/flags/reserved fields, exact length, checksum, canonical
  structure and actual totals. A recomputed checksum must not bypass validation.
- Validate in an allocation-free first pass, counting actual pages/groups/postings
  and checking caller limits. Only then allocate and populate the immutable
  representation in a second pass. Do not collect a flat CTID vector and sort it.
- Estimate decoded structural storage as `group_count * size_of::<Group>() +
  page_count * size_of::<Page>() + posting_count * size_of::<u16>()`, using checked
  arithmetic; enforce the structural budget before allocation. Reserve only the
  exact validated element counts, with fallible `try_reserve_exact` calls.
- This is a limit on requested vector element storage, **not an RSS/allocator
  overhead guarantee**. Allocators may round requests; the borrowed input,
  returned object on the stack, process overhead and allocator metadata are not
  charged. Limits are not PostgreSQL memory-context or work_mem accounting.
- Decode work is linear in bounded input bytes plus validated output elements.
  No recursion, unsafe code, decompression bombs, new third-party dependencies or
  unchecked indexing into untrusted slices. There is no cryptographic guarantee.
- Encode accepts an already validated in-memory set. Check u32/u64 field limits,
  total output size, arithmetic overflow and allocation errors; it has no separate
  caller budget and produces a complete output vector.

## Deferred integration decisions

Metapages, page checksums, extents, segment publication/generation pinning, WAL,
crash/replay, HOT-root selection, liveness, dictionary/position streams and
maintenance are separate design work. Do not persist these bytes as a live SQL
index merely because they can round-trip. PostgreSQL-managed storage must add its
own ownership, locking, atomicity, resource and recovery contracts.

The portable scalar format is normative for this experiment. Future accelerators
must produce identical canonical bytes and results. Future format changes must
bump the format version and explicitly reject unsupported versions, never silently
reinterpret v1 data. No upgrade implementation is supplied in this milestone.
