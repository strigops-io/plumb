// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Plumb contributors
//! Experimental MAIN-fork immutable postings storage. Callers must hold the
//! relation lock that prevents concurrent truncation/reindex, and enter through
//! a pgrx-guarded PostgreSQL callback. No callback into user code holds a pin.
//!
//! The envelope is explicitly little-endian, independent of Rust struct layout.
//! Each segment occupies consecutive newly extended blocks; its descriptor is
//! repeated on each chunk. Roots link strictly backwards. Unpublished extensions
//! are harmless orphans, bounded by a separate physical cap; there is no reclaim.

use pgrx::pg_sys;
use std::{mem::offset_of, ptr, slice};

pub const MAX_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SEGMENTS: u32 = 65_536;
const MAX_PHYSICAL_BYTES: usize = 256 * 1024 * 1024;
const BLOCK_SIZE: usize = pg_sys::BLCKSZ as usize;
const MAX_BLOCKS: u32 = (MAX_PHYSICAL_BYTES / BLOCK_SIZE) as u32;
// PostgreSQL's SizeOfPageHeaderData is offsetof(PageHeaderData, pd_linp).
const PAGE_HEADER: usize = offset_of!(pg_sys::PageHeaderData, pd_linp);
const META_SIZE: usize = 40;
const CHUNK_HEADER: usize = 48;
const CHUNK_CAPACITY: usize = BLOCK_SIZE - PAGE_HEADER - CHUNK_HEADER;
const VERSION: u32 = 1;
const NONE: u32 = pg_sys::InvalidBlockNumber;
const META_MAGIC: &[u8; 8] = b"PLMBMETA";
const CHUNK_MAGIC: &[u8; 8] = b"PLMBSEGM";

#[derive(Debug, Clone, Copy)]
pub struct StorageStats {
    pub format_version: u32,
    pub segments: u32,
    pub payload_bytes: u64,
    pub relation_blocks: u32,
}

#[derive(Clone, Copy)]
struct Meta {
    head: u32,
    segments: u32,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    root: u32,
    previous: u32,
    bytes: u32,
    pages: u32,
}

fn corrupt(message: &str) -> ! {
    pgrx::error!(
        "plumb postings storage is corrupt or unsupported: {}",
        message
    )
}

/// A pin is owned immediately, before lock acquisition can throw. pgrx turns PG
/// ERRORs into unwinding at its FFI boundary; normal Rust errors also run Drop.
/// PostgreSQL resource-owner cleanup remains the backstop for PG error exits.
struct Buffer {
    id: pg_sys::Buffer,
    locked: bool,
}
impl Buffer {
    unsafe fn read(index: pg_sys::Relation, block: u32, exclusive: bool) -> Self {
        unsafe {
            let id = pg_sys::ReadBufferExtended(
                index,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                block,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                ptr::null_mut(),
            );
            let mut buffer = Self { id, locked: false };
            pg_sys::LockBuffer(
                id,
                if exclusive {
                    pg_sys::BUFFER_LOCK_EXCLUSIVE
                } else {
                    pg_sys::BUFFER_LOCK_SHARE
                } as i32,
            );
            buffer.locked = true;
            buffer
        }
    }

    unsafe fn extend(index: pg_sys::Relation, expected: u32) -> Self {
        unsafe {
            // Writers are serialized by block zero (or build's exclusive relation
            // ownership), so no other legitimate writer may allocate this block.
            let id = pg_sys::ReadBufferExtended(
                index,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                pg_sys::InvalidBlockNumber,
                pg_sys::ReadBufferMode::RBM_ZERO_AND_LOCK,
                ptr::null_mut(),
            );
            let buffer = Self { id, locked: true };
            if pg_sys::BufferGetBlockNumber(id) != expected {
                corrupt("unexpected concurrent relation extension");
            }
            buffer
        }
    }

    unsafe fn contents(&self) -> &[u8] {
        unsafe {
            let page = pg_sys::BufferGetPage(self.id);
            let h = &*page.cast::<pg_sys::PageHeaderData>();
            let lower = h.pd_lower as usize;
            // Do not derive slices or trust descriptors until the PG header is
            // checked. Payload lives before pd_lower, never in the WAL hole.
            if h.pd_pagesize_version != (pg_sys::BLCKSZ | pg_sys::PG_PAGE_LAYOUT_VERSION) as u16
                || h.pd_flags != 0
                || h.pd_upper as usize != BLOCK_SIZE
                || h.pd_special as usize != BLOCK_SIZE
                || h.pd_prune_xid != pg_sys::InvalidTransactionId
                || !(PAGE_HEADER..=BLOCK_SIZE).contains(&lower)
            {
                corrupt("invalid PostgreSQL page header");
            }
            slice::from_raw_parts(page.cast::<u8>().add(PAGE_HEADER), lower - PAGE_HEADER)
        }
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            if self.locked {
                pg_sys::UnlockReleaseBuffer(self.id);
            } else {
                pg_sys::ReleaseBuffer(self.id);
            }
        }
    }
}

struct Wal(*mut pg_sys::GenericXLogState);
impl Drop for Wal {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { pg_sys::GenericXLogAbort(self.0) };
        }
    }
}

/// All initialized pages, including the first metapage and subsequent head
/// publications, get full-image Generic WAL. There is no unlogged PageInit.
unsafe fn write_page(index: pg_sys::Relation, buffer: &Buffer, body: &[u8]) {
    if body.len() > BLOCK_SIZE - PAGE_HEADER {
        corrupt("internal page envelope exceeds block size");
    }
    unsafe {
        let mut wal = Wal(pg_sys::GenericXLogStart(index));
        let page = pg_sys::GenericXLogRegisterBuffer(
            wal.0,
            buffer.id,
            pg_sys::GENERIC_XLOG_FULL_IMAGE as i32,
        );
        pg_sys::PageInit(page, BLOCK_SIZE, 0);
        ptr::copy_nonoverlapping(
            body.as_ptr(),
            page.cast::<u8>().add(PAGE_HEADER),
            body.len(),
        );
        (*page.cast::<pg_sys::PageHeaderData>()).pd_lower = (PAGE_HEADER + body.len()) as u16;
        let state = wal.0;
        // Finish owns/frees the state. On a PostgreSQL error its memory context
        // is reclaimed, so Drop must not try to abort a potentially freed state.
        wal.0 = ptr::null_mut();
        pg_sys::GenericXLogFinish(state);
    }
}

fn get32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}
const CRC_TABLE: [u32; 256] = crc_table();
// IEEE CRC32 covers every envelope field and payload byte, excluding only the
// stored CRC itself. PostgreSQL's optional physical page checksum is separate.
fn checksum(header: &[u8], payload: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in header.iter().chain(payload) {
        crc = (crc >> 8) ^ CRC_TABLE[((crc ^ byte as u32) & 255) as usize];
    }
    !crc
}

fn encode_meta(meta: Meta) -> [u8; META_SIZE] {
    let mut body = [0; META_SIZE];
    body[..8].copy_from_slice(META_MAGIC);
    put32(&mut body, 8, VERSION);
    put32(&mut body, 12, META_SIZE as u32);
    put32(&mut body, 16, meta.head);
    put32(&mut body, 20, meta.segments);
    body[24..32].copy_from_slice(&meta.bytes.to_le_bytes());
    let crc = checksum(&body[..36], &[]);
    put32(&mut body, 36, crc);
    body
}

fn decode_meta(body: &[u8], blocks: u32) -> Result<Meta, &'static str> {
    if body.len() != META_SIZE
        || &body[..8] != META_MAGIC
        || get32(body, 8) != VERSION
        || get32(body, 12) != META_SIZE as u32
        || get32(body, 32) != 0
        || get32(body, 36) != checksum(&body[..36], &[])
    {
        return Err("invalid metapage magic/version/length/checksum");
    }
    let meta = Meta {
        head: get32(body, 16),
        segments: get32(body, 20),
        bytes: u64::from_le_bytes(body[24..32].try_into().unwrap()),
    };
    if blocks == 0
        || blocks > MAX_BLOCKS
        || meta.segments > MAX_SEGMENTS
        || meta.bytes > MAX_INDEX_BYTES as u64
    {
        return Err("metapage exceeds storage bounds");
    }
    if meta.segments == 0 {
        if meta.head != NONE || meta.bytes != 0 {
            return Err("nonempty head/bytes with zero segments");
        }
    } else if meta.head == 0
        || meta.head >= blocks
        || meta.segments > meta.head
        || meta.bytes < meta.segments as u64
        || meta.bytes > meta.segments as u64 * MAX_SEGMENT_BYTES as u64
    {
        return Err("invalid metapage head/count/byte totals");
    }
    Ok(meta)
}

fn page_count(bytes: usize) -> u32 {
    bytes.div_ceil(CHUNK_CAPACITY) as u32
}

fn encode_chunk(segment: Segment, ordinal: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0; CHUNK_HEADER + payload.len()];
    body[..8].copy_from_slice(CHUNK_MAGIC);
    put32(&mut body, 8, VERSION);
    put32(&mut body, 12, CHUNK_HEADER as u32);
    put32(&mut body, 16, segment.root);
    put32(&mut body, 20, segment.previous);
    put32(&mut body, 24, segment.bytes);
    put32(&mut body, 28, segment.pages);
    put32(&mut body, 32, ordinal);
    put32(&mut body, 36, payload.len() as u32);
    body[CHUNK_HEADER..].copy_from_slice(payload);
    let crc = checksum(&body[..44], payload);
    put32(&mut body, 44, crc);
    body
}

fn decode_chunk(
    body: &[u8],
    root: u32,
    ordinal: u32,
    upper: u32,
) -> Result<(Segment, &[u8]), &'static str> {
    if body.len() < CHUNK_HEADER
        || &body[..8] != CHUNK_MAGIC
        || get32(body, 8) != VERSION
        || get32(body, 12) != CHUNK_HEADER as u32
        || get32(body, 16) != root
        || get32(body, 32) != ordinal
        || get32(body, 40) != 0
    {
        return Err("invalid segment page magic/version/root/ordinal");
    }
    let segment = Segment {
        root,
        previous: get32(body, 20),
        bytes: get32(body, 24),
        pages: get32(body, 28),
    };
    if root == 0
        || root >= upper
        || segment.bytes == 0
        || segment.bytes as usize > MAX_SEGMENT_BYTES
        || segment.pages != page_count(segment.bytes as usize)
        || segment.pages > upper - root
        || ordinal >= segment.pages
        || (segment.previous != NONE && (segment.previous == 0 || segment.previous >= root))
    {
        return Err("invalid segment length/page count/backwards link");
    }
    let expected = (segment.bytes as usize - ordinal as usize * CHUNK_CAPACITY).min(CHUNK_CAPACITY);
    if get32(body, 36) as usize != expected
        || body.len() != CHUNK_HEADER + expected
        || get32(body, 44) != checksum(&body[..44], &body[CHUNK_HEADER..])
    {
        return Err("invalid segment chunk length/checksum");
    }
    Ok((segment, &body[CHUNK_HEADER..]))
}

unsafe fn blocks(index: pg_sys::Relation) -> u32 {
    unsafe { pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM) }
}
unsafe fn permanent(index: pg_sys::Relation) {
    unsafe {
        if index.is_null()
            || (*index).rd_rel.is_null()
            || (*(*index).rd_rel).relpersistence as u8 != pg_sys::RELPERSISTENCE_PERMANENT
        {
            pgrx::error!("plumb postings storage requires a permanent relation");
        }
    }
}

fn check_capacity(meta: Meta, payload: &[u8], existing_blocks: u32, extra_meta: u32) {
    if payload.len() > MAX_SEGMENT_BYTES {
        pgrx::error!("plumb postings storage capacity exceeded: segment limit is 16 MiB");
    }
    let count = meta.segments as u64 + u64::from(!payload.is_empty());
    if count > MAX_SEGMENTS as u64 || meta.bytes + payload.len() as u64 > MAX_INDEX_BYTES as u64 {
        pgrx::error!(
            "plumb postings storage capacity exceeded: 64 MiB payload or 65536 segments; REINDEX required (VACUUM does not reclaim postings)"
        );
    }
    if existing_blocks as u64 + extra_meta as u64 + page_count(payload.len()) as u64
        > MAX_BLOCKS as u64
    {
        pgrx::error!(
            "plumb postings storage capacity exceeded: 256 MiB physical relation limit (including orphan pages); REINDEX required"
        );
    }
}

/// Caller retains the exclusive metapage lock for the *entire* write and publish.
unsafe fn append_locked(
    index: pg_sys::Relation,
    meta_buffer: &Buffer,
    meta: Meta,
    payload: &[u8],
    current_blocks: u32,
) {
    check_capacity(meta, payload, current_blocks, 0);
    if payload.is_empty() {
        return;
    }
    let segment = Segment {
        root: current_blocks,
        previous: meta.head,
        bytes: payload.len() as u32,
        pages: page_count(payload.len()),
    };
    for (ordinal, chunk) in payload.chunks(CHUNK_CAPACITY).enumerate() {
        pgrx::check_for_interrupts!();
        let body = encode_chunk(segment, ordinal as u32, chunk);
        unsafe {
            let buffer = Buffer::extend(index, current_blocks + ordinal as u32);
            write_page(index, &buffer, &body);
        }
    }
    // Publication has its own record, following every referenced page's record.
    pgrx::check_for_interrupts!();
    let body = encode_meta(Meta {
        head: segment.root,
        segments: meta.segments + 1,
        bytes: meta.bytes + payload.len() as u64,
    });
    unsafe { write_page(index, meta_buffer, &body) };
}

/// Initialize an exclusively owned, zero-block relation. An empty build has only
/// a metapage. A zero-block relation must never be lazily initialized by append.
///
/// # Safety
/// `index` must be a live permanent index Relation, exclusively owned by build.
pub unsafe fn build(index: pg_sys::Relation, payload: &[u8]) {
    unsafe {
        permanent(index);
        if blocks(index) != 0 {
            pgrx::error!("plumb postings build requires a zero-block relation");
        }
        let meta = Meta {
            head: NONE,
            segments: 0,
            bytes: 0,
        };
        // Check all limits before even allocating the initial metapage.
        check_capacity(meta, payload, 0, 1);
        pgrx::check_for_interrupts!();
        let buffer = Buffer::extend(index, 0);
        write_page(index, &buffer, &encode_meta(meta));
        append_locked(index, &buffer, meta, payload, 1);
    }
}

/// # Safety
/// Caller must hold a relation lock preventing truncation and call from a
/// pgrx-guarded entry point. The relation must already have valid postings meta.
pub unsafe fn append(index: pg_sys::Relation, payload: &[u8]) {
    unsafe {
        permanent(index);
        if blocks(index) == 0 {
            corrupt("missing metapage on append; REINDEX required");
        }
        let buffer = Buffer::read(index, 0, true);
        let count = blocks(index);
        let meta = decode_meta(buffer.contents(), count).unwrap_or_else(|e| corrupt(e));
        append_locked(index, &buffer, meta, payload, count);
    }
}

unsafe fn snapshot(index: pg_sys::Relation) -> Option<(Meta, u32)> {
    unsafe {
        if blocks(index) == 0 {
            return None;
        }
        let buffer = Buffer::read(index, 0, false);
        // Read relation size under the same lock as the committed head/counters.
        let count = blocks(index);
        let meta = decode_meta(buffer.contents(), count).unwrap_or_else(|e| corrupt(e));
        Some((meta, count))
    }
}

/// Metadata-only inspection, not a full chain audit. Only zero physical blocks
/// return None; any nonempty relation must have a valid versioned metapage.
///
/// # Safety
/// Caller must hold a relation lock preventing truncation; `index` must be live.
pub unsafe fn stats(index: pg_sys::Relation) -> Option<StorageStats> {
    unsafe {
        snapshot(index).map(|(meta, count)| StorageStats {
            format_version: VERSION,
            segments: meta.segments,
            payload_bytes: meta.bytes,
            relation_blocks: count,
        })
    }
}

/// Traverse a bounded, committed head snapshot, newest first. A later corruption
/// raises ERROR even if earlier callbacks ran; callers must not expose partial
/// query results. Each callback runs after all buffer pins/locks are released.
///
/// # Safety
/// Caller must hold a relation lock preventing truncation and must call through
/// a pgrx-guarded entry point. Visitor is Rust code, not an unguarded C callback.
pub unsafe fn visit(index: pg_sys::Relation, mut visitor: impl FnMut(&[u8])) {
    unsafe {
        let (meta, mut upper) = snapshot(index)
            .unwrap_or_else(|| corrupt("missing metapage on visit; REINDEX required"));
        let mut root = meta.head;
        let mut total = 0u64;
        for segment_index in 0..meta.segments {
            pgrx::check_for_interrupts!();
            if root == NONE || root == 0 || root >= upper {
                corrupt("segment chain ended early or has an invalid/cyclic link");
            }
            let (segment, mut payload) = {
                let buffer = Buffer::read(index, root, false);
                let (segment, chunk) =
                    decode_chunk(buffer.contents(), root, 0, upper).unwrap_or_else(|e| corrupt(e));
                if segment.bytes as u64 > meta.bytes - total {
                    corrupt("segment chain exceeds committed byte total");
                }
                let mut payload = Vec::with_capacity(segment.bytes as usize);
                payload.extend_from_slice(chunk);
                (segment, payload)
            };
            for ordinal in 1..segment.pages {
                pgrx::check_for_interrupts!();
                let buffer = Buffer::read(index, root + ordinal, false);
                let (descriptor, chunk) = decode_chunk(buffer.contents(), root, ordinal, upper)
                    .unwrap_or_else(|e| corrupt(e));
                if descriptor != segment {
                    corrupt("segment descriptors disagree across chunks");
                }
                payload.extend_from_slice(chunk);
            }
            total += payload.len() as u64;
            if payload.len() != segment.bytes as usize {
                corrupt("reassembled segment length mismatch");
            }
            let last = segment_index + 1 == meta.segments;
            if last != (segment.previous == NONE) || (last && total != meta.bytes) {
                corrupt("segment chain count/byte total mismatch");
            }
            upper = root; // Also rejects page ranges overlapping a younger segment.
            root = segment.previous;
            visitor(&payload);
        }
        if root != NONE || total != meta.bytes {
            corrupt("segment chain count/byte total mismatch");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_crc_and_meta_validation() {
        assert_eq!(checksum(b"123456789", &[]), 0xcbf4_3926);
        let mut meta = encode_meta(Meta {
            head: NONE,
            segments: 0,
            bytes: 0,
        });
        assert!(decode_meta(&meta, 1).is_ok());
        meta[24] ^= 1;
        assert!(decode_meta(&meta, 1).is_err());
        assert!(decode_meta(&[], 1).is_err());
    }

    #[test]
    fn meta_semantic_bounds_even_with_valid_crc() {
        for meta in [
            Meta {
                head: 0,
                segments: 1,
                bytes: 1,
            },
            Meta {
                head: 1,
                segments: 0,
                bytes: 0,
            },
            Meta {
                head: NONE,
                segments: 0,
                bytes: 1,
            },
            Meta {
                head: 1,
                segments: 2,
                bytes: 2,
            },
            Meta {
                head: 1,
                segments: 1,
                bytes: 0,
            },
            Meta {
                head: 1,
                segments: 1,
                bytes: MAX_SEGMENT_BYTES as u64 + 1,
            },
        ] {
            assert!(decode_meta(&encode_meta(meta), 3).is_err());
        }
        let empty = encode_meta(Meta {
            head: NONE,
            segments: 0,
            bytes: 0,
        });
        // Orphans are allowed, but cannot evade the physical relation bound.
        assert!(decode_meta(&empty, MAX_BLOCKS).is_ok());
        assert!(decode_meta(&empty, MAX_BLOCKS + 1).is_err());
        let mut unsupported = empty;
        put32(&mut unsupported, 8, VERSION + 1);
        let crc = checksum(&unsupported[..36], &[]);
        put32(&mut unsupported, 36, crc);
        assert!(decode_meta(&unsupported, 1).is_err());
    }

    #[test]
    fn chunk_semantic_bounds_even_with_valid_crc() {
        let valid = Segment {
            root: 2,
            previous: 1,
            bytes: 1,
            pages: 1,
        };
        for bad in [
            Segment {
                previous: 2,
                ..valid
            },
            Segment {
                previous: 0,
                ..valid
            },
            Segment { bytes: 0, ..valid },
            Segment {
                bytes: MAX_SEGMENT_BYTES as u32 + 1,
                ..valid
            },
            Segment {
                pages: u32::MAX,
                ..valid
            },
        ] {
            assert!(decode_chunk(&encode_chunk(bad, 0, &[42]), 2, 0, 3).is_err());
        }
        let body = encode_chunk(valid, 0, &[42]);
        for length in 0..body.len() {
            assert!(decode_chunk(&body[..length], 2, 0, 3).is_err());
        }
        // Every one-byte mutation in descriptor, CRC or payload is detected.
        for offset in 0..body.len() {
            let mut damaged = body.clone();
            damaged[offset] ^= 1;
            assert!(decode_chunk(&damaged, 2, 0, 3).is_err());
        }
    }

    #[test]
    fn chunk_roundtrip_and_fail_closed() {
        let segment = Segment {
            root: 2,
            previous: 1,
            bytes: CHUNK_CAPACITY as u32 + 3,
            pages: 2,
        };
        let first = encode_chunk(segment, 0, &vec![7; CHUNK_CAPACITY]);
        let last = encode_chunk(segment, 1, &[8, 9, 10]);
        assert_eq!(decode_chunk(&first, 2, 0, 4).unwrap().0, segment);
        assert_eq!(decode_chunk(&last, 2, 1, 4).unwrap().1, &[8, 9, 10]);
        assert!(decode_chunk(&first, 2, 0, 3).is_err()); // overlapping next root
        assert!(decode_chunk(&first, 3, 0, 5).is_err()); // wrong root
        assert!(decode_chunk(&first, 2, 1, 4).is_err()); // wrong ordinal
        let mut damaged = last.clone();
        damaged[CHUNK_HEADER] ^= 1;
        assert!(decode_chunk(&damaged, 2, 1, 4).is_err());
        assert!(decode_chunk(&last[..last.len() - 1], 2, 1, 4).is_err());
    }
}
