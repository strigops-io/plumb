// SPDX-License-Identifier: AGPL-3.0-or-later
// Added by Plumb contributors on 2026-09-20: experimental bounded term segments.
//! Immutable term dictionary + scalar grouped-CTID frames. No visibility authority.
use plumb_postings::{Ctid, Postings, codec};
use std::collections::{BTreeMap, BTreeSet};
use tinql::runtime::Query;
use tokenizer::{Tokenizer, presets::default_pipeline};

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PAIRS: usize = 256 * 1024;
pub const MAX_QUERY_BYTES: usize = 1024;
const SOFT_PAIRS: usize = 65_536;
const SOFT_BYTES: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 256;
const MAGIC: &[u8; 8] = b"PLMBTRM\0";

// Host-side scalar unit tests do not have a PostgreSQL interrupt context.
// The extension (including pg_test SQL builds) always retains these checks.
#[inline]
fn check_interrupts() {
    #[cfg(not(test))]
    pgrx::check_for_interrupts!();
}

/// One bounded, sorted, unique analysis result, reusable after flushing a builder.
/// Private fields prevent bypassing preparation's token and resource invariants.
#[derive(Debug)]
pub struct PreparedDocument {
    terms: Vec<String>,
    tid: Ctid,
}

/// Analyze before touching a builder. The logical budget counts unique UTF-8 term
/// bytes plus one CTID per term, not repeated tokens or allocator bookkeeping.
///
/// Memory is bounded, but 16 MiB is NOT an RSS/work_mem promise: preparation has
/// at most MAX_PAIRS BTreeSet entries, then MAX_PAIRS String headers while moving
/// them to a Vec. A live prepared document and builder can coexist. Builder map
/// nodes/String/Vec headers and geometric vector spare capacity are additional
/// O(MAX_PAIRS) storage; the default tokenizer's scratch is bounded by the 16 MiB
/// input. No token occurrence list or unbounded corpus copy is retained here.
pub fn prepare(document: &str, tid: Ctid) -> Result<PreparedDocument, String> {
    if document.len() > MAX_BYTES {
        return Err("postings_v1 document exceeds 16 MiB text limit".into());
    }
    let mut terms = BTreeSet::new();
    let mut logical_bytes = 0;
    check_interrupts();
    for (n, token) in default_pipeline().tokenize(document).enumerate() {
        if n % 1024 == 0 {
            check_interrupts();
        }
        let term = token.text.as_ref();
        if term.is_empty() || term.len() > 256 {
            return Err("noncanonical postings_v1 document token length".into());
        }
        if terms.contains(term) {
            continue;
        }
        let additional = term.len() + std::mem::size_of::<Ctid>();
        if terms.len() >= MAX_PAIRS || logical_bytes + additional > MAX_BYTES {
            return Err("postings_v1 prepared document exceeds 16 MiB logical term strings/CTIDs or 262144 unique term/CTID pairs".into());
        }
        // Check both limits before allocating the next retained term.
        logical_bytes += additional;
        terms.insert(term.to_owned());
    }
    Ok(PreparedDocument {
        terms: terms.into_iter().collect(),
        tid,
    })
}

#[derive(Default)]
pub struct Builder {
    terms: BTreeMap<String, Vec<Ctid>>,
    logical_bytes: usize,
    pairs: usize,
    // Upper bound excluding the 36-byte v2 dictionary envelope. CTID page/group
    // runs overcount revisits, but equal the codec cost for ordered heap scans.
    wire_bytes: usize,
}

// v1 frame: 40-byte header, 36 per group, 2 per page, 2 per offset.
// Each distinct page/group has at least one run even for unordered CTIDs, so
// charging transitions is a safe bound without rescanning or cloning postings.
// A common term on the same page costs just two bytes per additional CTID.
fn posting_wire_growth(previous: Option<Ctid>, tid: Ctid) -> usize {
    if previous == Some(tid) {
        return 0;
    }
    2 + if previous.is_none() { 40 } else { 0 }
        + if previous.is_none_or(|old| old.block() != tid.block()) {
            2
        } else {
            0
        }
        + if previous.is_none_or(|old| old.block() / 256 != tid.block() / 256) {
            36
        } else {
            0
        }
}

impl Builder {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// A soft target, not an admission limit: a whole document may cross it.
    pub fn should_flush(&self) -> bool {
        self.pairs >= SOFT_PAIRS || self.logical_bytes >= SOFT_BYTES
    }

    fn projection(&self, document: &PreparedDocument) -> Option<(usize, usize, usize)> {
        let mut pairs = self.pairs;
        let mut bytes = self.logical_bytes;
        let mut wire_bytes = self.wire_bytes;
        for (n, term) in document.terms.iter().enumerate() {
            if n % 1024 == 0 {
                check_interrupts();
            }
            let existing = self.terms.get(term);
            // Keep upstream's last-CTID fast path. Nonconsecutive duplicates
            // conservatively consume capacity until finish canonicalizes them.
            if existing.is_some_and(|tids| tids.last() == Some(&document.tid)) {
                continue;
            }
            pairs = pairs.checked_add(1)?;
            bytes = bytes.checked_add(
                std::mem::size_of::<Ctid>() + if existing.is_none() { term.len() } else { 0 },
            )?;
            wire_bytes = wire_bytes.checked_add(
                posting_wire_growth(existing.and_then(|tids| tids.last().copied()), document.tid)
                    + if existing.is_none() {
                        16 + term.len()
                    } else {
                        0
                    },
            )?;
            if pairs > MAX_PAIRS || bytes > MAX_BYTES || wire_bytes > MAX_BYTES - 36 {
                return None;
            }
        }
        Some((pairs, bytes, wire_bytes))
    }

    /// Pre-add flush hint as well as hard admission. A whole document can exceed
    /// a soft target in an empty builder; should_flush handles already-full ones.
    /// add_prepared independently enforces all hard bounds even if this hint is
    /// ignored, preserving the soft-target (rather than hard-limit) API.
    pub fn can_accept(&self, document: &PreparedDocument) -> bool {
        self.projection(document).is_some_and(|(pairs, bytes, _)| {
            self.is_empty() || self.should_flush() || (pairs <= SOFT_PAIRS && bytes <= SOFT_BYTES)
        })
    }

    /// A returned error leaves every builder field unchanged. Preflight the
    /// entire unique document, not individual tokens, before the first mutation.
    pub fn add_prepared(&mut self, document: &PreparedDocument) -> Result<(), String> {
        let (pairs, logical_bytes, wire_bytes) = self.projection(document).ok_or(
            "postings_v1 builder limit exceeded (16 MiB logical term strings/CTIDs, 16 MiB encoded term segment, or 262144 term/CTID pairs); flush and retry the prepared document",
        )?;
        for term in &document.terms {
            if let Some(tids) = self.terms.get_mut(term) {
                if tids.last() != Some(&document.tid) {
                    tids.push(document.tid);
                }
            } else {
                self.terms.insert(term.clone(), vec![document.tid]);
            }
        }
        self.pairs = pairs;
        self.logical_bytes = logical_bytes;
        self.wire_bytes = wire_bytes;
        Ok(())
    }

    pub fn add(&mut self, document: &str, tid: Ctid) -> Result<(), String> {
        self.add_prepared(&prepare(document, tid)?)
    }

    pub fn finish(self) -> Result<Vec<u8>, String> {
        if self.terms.is_empty() {
            return Ok(Vec::new());
        }
        let term_count = self.terms.len();
        let directory_len = V2_HEADER + 4 + self.terms.keys().map(|t| 16 + t.len()).sum::<usize>();
        if directory_len > MAX_BYTES {
            return Err("v2 directory exceeds 16 MiB".into());
        }
        let mut directory = vec![0; V2_HEADER];
        let mut frames = Vec::new();
        for (term, tids) in self.terms {
            check_interrupts();
            let postings = Postings::from_ctids(tids);
            let frame = codec::encode(&postings).map_err(|e| e.to_string())?;
            let offset = directory_len + frames.len();
            if offset + frame.len() > MAX_BYTES {
                return Err("postings_v1 encoded term segment exceeds 16 MiB".into());
            }
            for value in [term.len(), offset, frame.len(), postings.len()] {
                directory.extend_from_slice(&(value as u32).to_le_bytes());
            }
            directory.extend_from_slice(term.as_bytes());
            frames.extend_from_slice(&frame);
        }
        directory[..8].copy_from_slice(MAGIC);
        for (at, value) in [
            (8, 2),
            (12, term_count as u32),
            (16, (directory_len + frames.len()) as u32),
            (20, directory_len as u32),
        ] {
            directory[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        let header_crc = crc32c(&directory[..28]);
        directory[28..32].copy_from_slice(&header_crc.to_le_bytes());
        let directory_crc = crc32c(&directory);
        directory.extend_from_slice(&directory_crc.to_le_bytes());
        let mut bytes = directory;
        bytes.extend_from_slice(&frames);
        Ok(bytes)
    }
}

// Castagnoli checksum: v1 covers the whole payload; v2 has independent fixed
// header and complete directory checksums. Posting frames retain their own CRC.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = plumb_postings::accel::Crc32c::new();
    for chunk in bytes.chunks(65536) {
        check_interrupts();
        crc.update(chunk);
    }
    crc.finish()
}

fn take<'a>(data: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if n > data.len() {
        return Err("truncated postings_v1 term segment".into());
    }
    let (out, rest) = data.split_at(n);
    *data = rest;
    Ok(out)
}
fn u32le(data: &mut &[u8]) -> Result<u32, String> {
    Ok(u32::from_le_bytes(take(data, 4)?.try_into().unwrap()))
}

/// Walk the entire validated dictionary/envelope. The caller decides which
/// frames to decode; merge must decode every frame, unlike query accumulation.
fn visit_frames_v1(
    bytes: &[u8],
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    if !(24..=MAX_BYTES).contains(&bytes.len()) {
        return Err("invalid postings_v1 term segment length".into());
    }
    let (body, checksum) = bytes.split_at(bytes.len() - 4);
    if crc32c(body) != u32::from_le_bytes(checksum.try_into().unwrap()) {
        return Err("postings_v1 term segment CRC32C mismatch (dictionary and frames)".into());
    }
    let mut data = body;
    if take(&mut data, 8)? != MAGIC || u32le(&mut data)? != 1 {
        return Err("unsupported postings_v1 term segment magic/version".into());
    }
    let count = u32le(&mut data)? as usize;
    if count == 0 || count > MAX_PAIRS || u32le(&mut data)? as usize != bytes.len() {
        return Err("invalid postings_v1 term count/length".into());
    }
    let mut previous = "";
    for _ in 0..count {
        check_interrupts();
        let term_len = u32le(&mut data)? as usize;
        let frame_len = u32le(&mut data)? as usize;
        let term = std::str::from_utf8(take(&mut data, term_len)?)
            .map_err(|_| "non-UTF8 postings_v1 term")?;
        if term.is_empty() || term.len() > 256 || term <= previous {
            return Err("noncanonical postings_v1 dictionary (term length/order)".into());
        }
        previous = term;
        visit(term, take(&mut data, frame_len)?)?;
    }
    if !data.is_empty() {
        return Err("trailing postings_v1 term segment bytes".into());
    }
    Ok(())
}

fn decode_frame(frame: &[u8], remaining_pairs: usize) -> Result<Postings, String> {
    codec::decode(
        frame,
        codec::DecodeLimits {
            max_encoded_bytes: MAX_BYTES,
            max_postings: remaining_pairs,
            max_groups: remaining_pairs,
            max_pages: remaining_pairs,
            max_decoded_bytes: 64 * 1024 * 1024,
        },
    )
    .map_err(|e| format!("postings_v1 frame: {e}"))
}

/// Full-memory compatibility/test path: v1 validates the whole envelope and
/// matching frames, v2 validates every frame. Executor uses accumulate_ranges.
pub fn accumulate(bytes: &[u8], terms: &mut BTreeMap<String, Postings>) -> Result<(), String> {
    visit_frames(bytes, |term, frame| {
        if let Some(existing) = terms.get_mut(term) {
            let decoded = decode_frame(frame, MAX_PAIRS)?;
            validate_heap_offsets(&decoded)?;
            // Bound before allocation: conservative even if sets overlap.
            if existing.len() + decoded.len() > MAX_PAIRS {
                return Err(
                    "postings_v1 query candidate limit exceeded (262144 CTIDs per term)".into(),
                );
            }
            *existing = existing.union(&decoded);
        }
        if terms.values().map(Postings::len).sum::<usize>() > MAX_PAIRS {
            return Err(
                "postings_v1 query materialization limit exceeded (262144 term/CTID pairs)".into(),
            );
        }
        Ok(())
    })
}

// PostgreSQL's MaxHeapTuplesPerPage: a physical format bound only, not a
// statement about whether a tuple currently exists or is visible in the heap.
fn max_heap_offset() -> usize {
    use pgrx::pg_sys;
    let alignment = pg_sys::MAXIMUM_ALIGNOF as usize;
    let tuple_header = std::mem::offset_of!(pg_sys::HeapTupleHeaderData, t_bits);
    let aligned_header = (tuple_header + alignment - 1) & !(alignment - 1);
    (pg_sys::BLCKSZ as usize - std::mem::offset_of!(pg_sys::PageHeaderData, pd_linp))
        / (aligned_header + std::mem::size_of::<pg_sys::ItemIdData>())
}

// Validate every selected frame before any union/Boolean operation, even when
// another term or scan key will make the final candidate bitmap empty.
fn validate_heap_offsets(decoded: &Postings) -> Result<(), String> {
    let max_offset = max_heap_offset();
    for (n, tid) in decoded.iter().enumerate() {
        if n % 1024 == 0 {
            check_interrupts();
        }
        if usize::from(tid.offset()) > max_offset {
            return Err("postings_v1 frame contains an impossible heap tuple offset".into());
        }
    }
    Ok(())
}

/// Consolidate physical candidates only: no visibility/age/heap decisions.
/// All dictionaries AND all codec frames are validated, including unqueried terms.
///
/// The cumulative decoded input pair cap is conservative even for duplicates;
/// enforce it in the codec before allocation, not after constructing a union.
/// The borrowed aggregate input and returned encoding are each <=16 MiB. The
/// temporary map retains <=MAX_PAIRS CTIDs and <=MAX_PAIRS keys (key bytes <=
/// input bytes). Map headers/nodes, vector spare capacity, one decoded frame
/// (<=64 MiB structural storage), and finish's one-term canonicalization/encoding
/// scratch are additional bounded O(MAX_PAIRS) overhead, not charged as payload.
/// Nothing is externally published here; any error discards this local result.
pub fn merge_payloads(payloads: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut input_bytes = 0usize;
    for payload in payloads {
        check_interrupts();
        input_bytes = input_bytes
            .checked_add(payload.len())
            .filter(|&n| n <= MAX_BYTES)
            .ok_or("postings_v1 merge input exceeds 16 MiB")?;
    }
    let mut merged = Builder::default();
    for payload in payloads {
        visit_frames(payload, |term, frame| {
            let decoded = decode_frame(frame, MAX_PAIRS - merged.pairs)?;
            if decoded.is_empty() {
                return Err("noncanonical postings_v1 empty term frame".into());
            }
            validate_heap_offsets(&decoded)?;
            merged.pairs += decoded.len();
            // Appending duplicates is bounded by the *input* count. finish's
            // Postings::from_ctids sorts/unions each term into canonical v1.
            merged
                .terms
                .entry(term.to_owned())
                .or_default()
                .extend(decoded.iter());
            Ok(())
        })?;
    }
    merged.finish()
}

fn safe_expr(expr: &tinql::Expr) -> Result<bool, String> {
    let mut work = vec![(expr, 0)];
    let mut nodes = 0;
    while let Some((e, depth)) = work.pop() {
        nodes += 1;
        if depth > 64 || nodes > MAX_NODES {
            return Err("postings_v1 query exceeds 64 levels/256 nodes".into());
        }
        match e {
            tinql::Expr::Term(_) => (),
            tinql::Expr::And(a, b) | tinql::Expr::Or(a, b) => {
                work.push((a, depth + 1));
                work.push((b, depth + 1));
            }
            tinql::Expr::Boost { inner, .. } => work.push((inner, depth + 1)),
            _ => return Ok(false),
        }
    }
    Ok(true)
}

pub fn query(text: &str) -> Result<Option<Query>, String> {
    if text.len() > MAX_QUERY_BYTES {
        return Err("postings_v1 query exceeds 1024 byte limit".into());
    }
    // Bound nesting before invoking the inherited recursive parser. Counting all
    // delimiter characters is deliberately conservative, including quoted ones.
    if text
        .bytes()
        .filter(|c| matches!(c, b'(' | b'[' | b'{'))
        .count()
        > 64
    {
        return Err("postings_v1 query exceeds 64 nesting delimiters".into());
    }
    let parsed = tinql::parse(text, tinql::ImplicitOp::And).map_err(|e| e.to_string())?;
    if !safe_expr(&parsed)? {
        return Ok(None);
    }
    let analyzed = tinql::runtime::subtokenize::sub_tokenize(parsed, default_pipeline())
        .map_err(|e| e.to_string())?;
    if !safe_expr(&analyzed)? {
        return Ok(None);
    }
    tinql::runtime::lower::lower(&analyzed)
        .map(Some)
        .map_err(|e| e.to_string())
}

pub fn collect_terms(query: &Query, out: &mut BTreeMap<String, Postings>) {
    match query {
        Query::Term(term) => {
            out.entry(term.clone()).or_default();
        }
        Query::And(a, b) | Query::Or(a, b) => {
            collect_terms(a, out);
            collect_terms(b, out);
        }
        Query::Conjunction(children) | Query::Disjunction { min: 1, children } => {
            for child in children {
                collect_terms(child, out);
            }
        }
        Query::Boost { inner, .. } => collect_terms(inner, out),
        _ => unreachable!("only bounded positive queries admitted"),
    }
}

pub fn evaluate(query: &Query, terms: &BTreeMap<String, Postings>) -> Result<Postings, String> {
    evaluate_budget(query, terms, &mut (4 * MAX_PAIRS))
}

fn charge(budget: &mut usize, count: usize) -> Result<(), String> {
    *budget = budget
        .checked_sub(count)
        .ok_or("postings_v1 Boolean intermediate budget exceeded (1048576 cumulative CTIDs)")?;
    Ok(())
}

fn evaluate_budget(
    query: &Query,
    terms: &BTreeMap<String, Postings>,
    budget: &mut usize,
) -> Result<Postings, String> {
    let combine =
        |a: Postings, b: Postings, and: bool, budget: &mut usize| -> Result<Postings, String> {
            if !and && a.len() + b.len() > MAX_PAIRS {
                return Err("postings_v1 Boolean candidate limit exceeded".into());
            }
            charge(
                budget,
                if and {
                    a.len().min(b.len())
                } else {
                    a.len() + b.len()
                },
            )?;
            Ok(if and { a.intersection(&b) } else { a.union(&b) })
        };
    match query {
        Query::Term(term) => {
            charge(budget, terms[term].len())?;
            Ok(terms[term].clone())
        }
        Query::And(a, b) | Query::Or(a, b) => combine(
            evaluate_budget(a, terms, budget)?,
            evaluate_budget(b, terms, budget)?,
            matches!(query, Query::And(..)),
            budget,
        ),
        Query::Conjunction(children) | Query::Disjunction { min: 1, children } => {
            let mut iter = children.iter();
            let mut result = evaluate_budget(
                iter.next().expect("nonempty positive Boolean"),
                terms,
                budget,
            )?;
            for child in iter {
                result = combine(
                    result,
                    evaluate_budget(child, terms, budget)?,
                    matches!(query, Query::Conjunction(_)),
                    budget,
                )?;
            }
            Ok(result)
        }
        Query::Boost { inner, .. } => evaluate_budget(inner, terms, budget),
        _ => Err("unsupported postings_v1 query reached evaluator".into()),
    }
}

// Added by Plumb contributors on 2026-09-20: independently checked v2 directory.
const V2_HEADER: usize = 32;
#[derive(Debug)]
struct Entry {
    offset: usize,
    length: usize,
    count: usize,
}
#[derive(Debug)]
pub struct Directory {
    entries: BTreeMap<String, Entry>,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn v2_header(bytes: &[u8], total: usize) -> Result<(usize, usize), String> {
    if bytes.len() != V2_HEADER
        || &bytes[..8] != MAGIC
        || word(bytes, 8) != 2
        || word(bytes, 16) as usize != total
        || total > MAX_BYTES
        || word(bytes, 24) != 0
        || crc32c(&bytes[..28]) != word(bytes, 28)
    {
        return Err("invalid v2 term header/version/length/checksum".into());
    }
    let count = word(bytes, 12) as usize;
    let dir_len = word(bytes, 20) as usize;
    if count == 0
        || count > MAX_PAIRS
        || dir_len < V2_HEADER + 4
        || dir_len > total
        || count > (dir_len - V2_HEADER - 4) / 17
    {
        return Err("invalid v2 directory count/length".into());
    }
    Ok((count, dir_len))
}
fn parse_directory(bytes: &[u8], total: usize) -> Result<Directory, String> {
    if bytes.len() < V2_HEADER + 4 {
        return Err("truncated v2 directory".into());
    }
    let (count, dir_len) = v2_header(&bytes[..V2_HEADER], total)?;
    if bytes.len() != dir_len || crc32c(&bytes[..dir_len - 4]) != word(bytes, dir_len - 4) {
        return Err("v2 term directory CRC32C mismatch".into());
    }
    let mut data = &bytes[V2_HEADER..dir_len - 4];
    let mut entries = BTreeMap::new();
    let mut previous = String::new();
    let mut expected_offset = dir_len;
    let mut pairs = 0usize;
    for _ in 0..count {
        check_interrupts();
        let term_len = u32le(&mut data)? as usize;
        let offset = u32le(&mut data)? as usize;
        let length = u32le(&mut data)? as usize;
        let count = u32le(&mut data)? as usize;
        let term =
            std::str::from_utf8(take(&mut data, term_len)?).map_err(|_| "non-UTF8 v2 term")?;
        if term.is_empty()
            || term.len() > 256
            || term <= previous.as_str()
            || offset != expected_offset
            || length < 40
            || count == 0
            || count > MAX_PAIRS
        {
            return Err("noncanonical v2 term/order/offset/length/count".into());
        }
        expected_offset = offset
            .checked_add(length)
            .filter(|&n| n <= total)
            .ok_or("v2 frame range exceeds segment")?;
        pairs = pairs
            .checked_add(count)
            .filter(|&n| n <= MAX_PAIRS)
            .ok_or("v2 directory pair count exceeds bound")?;
        previous = term.to_owned();
        entries.insert(
            term.to_owned(),
            Entry {
                offset,
                length,
                count,
            },
        );
    }
    if !data.is_empty() || expected_offset != total {
        return Err("v2 directory trailing bytes/frame length mismatch".into());
    }
    Ok(Directory { entries })
}

pub enum LoadedDirectory {
    Legacy(Vec<u8>),
    V2(Directory),
}
impl LoadedDirectory {
    pub fn version(&self) -> u32 {
        match self {
            Self::Legacy(_) => 1,
            Self::V2(_) => 2,
        }
    }
}
/// Exactly one directory parse per segment for all query terms. v1 has a whole
/// payload checksum and necessarily retains its legacy full-read path.
pub fn load_directory(
    total: usize,
    read: impl FnMut(usize, usize) -> Result<Vec<u8>, String>,
) -> Result<LoadedDirectory, String> {
    load_directory_fallible(total, read)
}

/// Keep read errors typed for advisory callers; only parser errors become E.
pub fn load_directory_fallible<E: From<String>>(
    total: usize,
    mut read: impl FnMut(usize, usize) -> Result<Vec<u8>, E>,
) -> Result<LoadedDirectory, E> {
    if !(24..=MAX_BYTES).contains(&total) {
        return Err(String::from("invalid term segment size").into());
    }
    let prefix = read(0, 12)?;
    if &prefix[..8] != MAGIC {
        return Err(String::from("unsupported term segment magic").into());
    }
    match word(&prefix, 8) {
        1 => {
            let bytes = read(0, total)?;
            visit_frames_v1(&bytes, |_, frame| {
                decode_frame(frame, MAX_PAIRS).map(|_| ())
            })?;
            Ok(LoadedDirectory::Legacy(bytes))
        }
        2 => {
            let header = read(0, V2_HEADER)?;
            let (_, length) = v2_header(&header, total)?;
            Ok(LoadedDirectory::V2(parse_directory(
                &read(0, length)?,
                total,
            )?))
        }
        _ => Err(String::from("unsupported term segment version").into()),
    }
}
fn add_frame(
    term: &str,
    frame: &[u8],
    count: Option<usize>,
    terms: &mut BTreeMap<String, Postings>,
) -> Result<(), String> {
    if let Some(existing) = terms.get_mut(term) {
        let decoded = decode_frame(frame, MAX_PAIRS)?;
        validate_heap_offsets(&decoded)?;
        if count.is_some_and(|n| n != decoded.len()) {
            return Err("v2 directory/frame posting count mismatch".into());
        }
        if existing.len() + decoded.len() > MAX_PAIRS {
            return Err(
                "postings_v1 query candidate limit exceeded (262144 CTIDs per term)".into(),
            );
        }
        *existing = existing.union(&decoded);
    }
    if terms.values().map(Postings::len).sum::<usize>() > MAX_PAIRS {
        return Err(
            "postings_v1 query materialization limit exceeded (262144 term/CTID pairs)".into(),
        );
    }
    Ok(())
}
pub fn accumulate_ranges(
    directory: &LoadedDirectory,
    mut read: impl FnMut(usize, usize) -> Result<Vec<u8>, String>,
    terms: &mut BTreeMap<String, Postings>,
) -> Result<(), String> {
    match directory {
        LoadedDirectory::Legacy(bytes) => accumulate(bytes, terms),
        LoadedDirectory::V2(directory) => {
            let keys: Vec<_> = terms.keys().cloned().collect();
            for term in keys {
                check_interrupts();
                if let Some(entry) = directory.entries.get(&term) {
                    add_frame(
                        &term,
                        &read(entry.offset, entry.length)?,
                        Some(entry.count),
                        terms,
                    )?;
                }
            }
            Ok(())
        }
    }
}
/// Advisory sums over physical segment histories, never snapshot-visible df.
pub fn physical_counts(
    directory: &LoadedDirectory,
    counts: &mut BTreeMap<String, u64>,
) -> Result<(), String> {
    match directory {
        LoadedDirectory::Legacy(bytes) => visit_frames_v1(bytes, |term, frame| {
            if let Some(count) = counts.get_mut(term) {
                *count += decode_frame(frame, MAX_PAIRS)?.len() as u64;
            }
            Ok(())
        }),
        LoadedDirectory::V2(directory) => {
            for (term, count) in counts {
                if let Some(entry) = directory.entries.get(term) {
                    *count += entry.count as u64;
                }
            }
            Ok(())
        }
    }
}
pub fn selected_frame_bytes(directory: &LoadedDirectory, counts: &BTreeMap<String, u64>) -> usize {
    match directory {
        LoadedDirectory::Legacy(_) => 0, // Already read in full.
        LoadedDirectory::V2(directory) => counts
            .keys()
            .filter_map(|term| directory.entries.get(term))
            .map(|entry| entry.length + 2 * 8192)
            .sum(), // conservative boundary pages
    }
}
pub fn count_upper_bound(query: &Query, counts: &BTreeMap<String, u64>) -> u64 {
    match query {
        Query::Term(term) => counts[term],
        Query::Boost { inner, .. } => count_upper_bound(inner, counts),
        Query::And(a, b) => count_upper_bound(a, counts).min(count_upper_bound(b, counts)),
        Query::Or(a, b) => {
            count_upper_bound(a, counts).saturating_add(count_upper_bound(b, counts))
        }
        Query::Conjunction(children) => children
            .iter()
            .map(|q| count_upper_bound(q, counts))
            .min()
            .unwrap_or(0),
        Query::Disjunction { min: 1, children } => children
            .iter()
            .map(|q| count_upper_bound(q, counts))
            .fold(0u64, u64::saturating_add),
        _ => unreachable!("only positive bounded queries"),
    }
}
fn visit_frames(
    bytes: &[u8],
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    if bytes.len() < 12 {
        return Err("truncated term header".into());
    }
    match word(bytes, 8) {
        1 => visit_frames_v1(bytes, visit),
        2 => {
            if bytes.len() < V2_HEADER {
                return Err("truncated v2 header".into());
            }
            let (_, n) = v2_header(&bytes[..V2_HEADER], bytes.len())?;
            let directory = parse_directory(&bytes[..n], bytes.len())?;
            for (term, entry) in directory.entries {
                check_interrupts();
                let frame = &bytes[entry.offset..entry.offset + entry.length];
                if decode_frame(frame, MAX_PAIRS)?.len() != entry.count {
                    return Err("v2 directory/frame posting count mismatch".into());
                }
                visit(&term, frame)?;
            }
            Ok(())
        }
        _ => Err("unsupported term segment version".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_impossible_offsets_fail_before_boolean_or_scan_key_intersection() {
        let maximum = u16::try_from(max_heap_offset()).unwrap();
        for version in [1, 2] {
            for impossible in [maximum + 1, u16::MAX] {
                let mut bytes = if version == 1 {
                    legacy_payload("beer", Ctid::new(0, maximum).unwrap())
                } else {
                    payload("beer", Ctid::new(0, maximum).unwrap())
                };
                let start = if version == 1 {
                    28 + "beer".len()
                } else {
                    word(&bytes, 20) as usize
                };
                let end = bytes.len() - if version == 1 { 4 } else { 0 };
                bytes[start + 78..start + 80].copy_from_slice(&impossible.to_le_bytes());
                let crc = crc32c(&[&bytes[start..start + 36], &bytes[start + 40..end]].concat());
                bytes[start + 36..start + 40].copy_from_slice(&crc.to_le_bytes());
                if version == 1 {
                    let crc = crc32c(&bytes[..end]);
                    bytes[end..].copy_from_slice(&crc.to_le_bytes());
                }
                // The frame and all enclosing checksums are valid: PG physical
                // offset validation, not codec/CRC rejection, must catch this.
                assert_eq!(
                    decode_frame(&bytes[start..end], MAX_PAIRS)
                        .unwrap()
                        .iter()
                        .next()
                        .unwrap()
                        .offset(),
                    impossible
                );
                let dir =
                    load_directory(bytes.len(), |at, n| Ok(bytes[at..at + n].to_vec())).unwrap();
                for keys in [
                    vec!["beer"],
                    vec!["beer AND absent"],
                    vec!["absent", "beer"],
                ] {
                    let queries: Vec<_> = keys
                        .into_iter()
                        .map(|text| query(text).unwrap().unwrap())
                        .collect();
                    let mut terms = BTreeMap::new();
                    for q in &queries {
                        collect_terms(q, &mut terms);
                    }
                    assert!(
                        evaluate(
                            &query("absent").unwrap().unwrap(),
                            &BTreeMap::from([("absent".into(), Postings::default())])
                        )
                        .unwrap()
                        .is_empty()
                    );
                    let error =
                        accumulate_ranges(&dir, |at, n| Ok(bytes[at..at + n].to_vec()), &mut terms)
                            .unwrap_err();
                    assert!(
                        error.contains("impossible heap tuple offset"),
                        "v{version}: {error}"
                    );
                    let error = accumulate(&bytes, &mut terms).unwrap_err();
                    assert!(error.contains("impossible heap tuple offset"));
                }
                // An unselected v2 frame remains deliberately outside validation.
                let mut absent = BTreeMap::from([("absent".into(), Postings::default())]);
                accumulate_ranges(
                    &dir,
                    |_, _| panic!("absent term must not read a frame"),
                    &mut absent,
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn advisory_directory_preserves_typed_read_budget_and_corruption() {
        use crate::storage::ReadError;
        for error in [
            ReadError::Budget("test byte budget"),
            ReadError::Corruption("test chunk CRC".into()),
        ] {
            let result = load_directory_fallible(100, |_, _| Err(error.clone()));
            assert!(matches!(result, Err(ref actual) if actual == &error));
        }
        let error = load_directory_fallible(100, |_, n| Ok::<_, ReadError>(vec![0; n]));
        assert!(matches!(error, Err(ReadError::Corruption(_))));
    }

    #[test]
    fn ranged_v2_absent_and_rare_read_only_directory_and_selected_frames() {
        let mut builder = one_term_builder(100_000);
        builder.add("zzrare", tid(100_001)).unwrap();
        let bytes = builder.finish().unwrap();
        let dir_len = word(&bytes, 20) as usize;
        for text in ["absent", "zzrare", "zzrare OR absent", "beer AND zzrare"] {
            let mut reads = Vec::new();
            let directory = load_directory(bytes.len(), |at, n| {
                reads.push((at, n));
                Ok(bytes[at..at + n].to_vec())
            })
            .unwrap();
            assert!(reads.iter().all(|(at, n)| at + n <= dir_len));
            let before = reads.len();
            let query = query(text).unwrap().unwrap();
            let mut terms = BTreeMap::new();
            collect_terms(&query, &mut terms);
            accumulate_ranges(
                &directory,
                |at, n| {
                    reads.push((at, n));
                    Ok(bytes[at..at + n].to_vec())
                },
                &mut terms,
            )
            .unwrap();
            assert_eq!(
                evaluate(&query, &terms).unwrap(),
                candidates(std::slice::from_ref(&bytes), text)
            );
            if text == "absent" {
                assert_eq!(reads.len(), before);
            }
            if text == "zzrare" || text == "zzrare OR absent" {
                assert_eq!(reads.len(), before + 1);
                assert!(reads.iter().map(|(_, n)| n).sum::<usize>() < bytes.len() / 100);
            }
        }
    }

    #[test]
    fn v2_unread_corruption_not_claimed_selected_corruption_rejected() {
        let mut bytes = payload("beer craft", tid(0));
        let directory =
            load_directory(bytes.len(), |at, n| Ok(bytes[at..at + n].to_vec())).unwrap();
        let LoadedDirectory::V2(ref dir) = directory else {
            panic!()
        };
        bytes[dir.entries["beer"].offset + 50] ^= 0x80;
        let mut terms = BTreeMap::from([("absent".into(), Postings::default())]);
        accumulate_ranges(
            &directory,
            |_, _| panic!("absent term read a posting frame"),
            &mut terms,
        )
        .unwrap();
        terms.insert("beer".into(), Postings::default());
        assert!(
            accumulate_ranges(
                &directory,
                |at, n| Ok(bytes[at..at + n].to_vec()),
                &mut terms
            )
            .is_err()
        );
        assert!(merge_payloads(&[bytes]).is_err());
    }

    fn reseal_v2(bytes: &mut [u8]) {
        let crc = crc32c(&bytes[..28]);
        bytes[28..32].copy_from_slice(&crc.to_le_bytes());
        let end = word(bytes, 20) as usize - 4;
        let crc = crc32c(&bytes[..end]);
        bytes[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    }
    #[test]
    fn v2_bad_offsets_counts_versions_and_directory_checksums_fail_closed() {
        let bytes = payload("beer craft", tid(0));
        let dir_len = word(&bytes, 20) as usize;
        for at in 0..dir_len {
            let mut bad = bytes.clone();
            bad[at] ^= 0x80;
            assert!(
                load_directory(bad.len(), |at, n| Ok(bad[at..at + n].to_vec())).is_err(),
                "directory byte {at}"
            );
        }
        for (at, value) in [
            (8, 3),
            (12, u32::MAX),
            (V2_HEADER + 4, 0),
            (V2_HEADER + 8, u32::MAX),
            (V2_HEADER + 12, 0),
            (V2_HEADER + 12, MAX_PAIRS as u32 + 1),
        ] {
            let mut bad = bytes.clone();
            bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
            reseal_v2(&mut bad);
            assert!(
                load_directory(bad.len(), |at, n| Ok(bad[at..at + n].to_vec())).is_err(),
                "resealed field {at}"
            );
        }
        let mut wrong_count = bytes.clone();
        wrong_count[V2_HEADER + 12..V2_HEADER + 16].copy_from_slice(&2u32.to_le_bytes());
        reseal_v2(&mut wrong_count);
        let directory = load_directory(wrong_count.len(), |at, n| {
            Ok(wrong_count[at..at + n].to_vec())
        })
        .unwrap();
        let mut terms = BTreeMap::from([("beer".into(), Postings::default())]);
        assert!(
            accumulate_ranges(
                &directory,
                |at, n| Ok(wrong_count[at..at + n].to_vec()),
                &mut terms
            )
            .unwrap_err()
            .contains("count mismatch")
        );
        assert!(merge_payloads(&[wrong_count]).is_err());
    }

    #[test]
    fn legacy_query_and_mixed_merge_write_v2_and_preserve_history_bounds() {
        let legacy = legacy_payload("beer", tid(0));
        let current = payload("craft beer", tid(0));
        let mut terms = BTreeMap::from([
            ("beer".into(), Postings::default()),
            ("craft".into(), Postings::default()),
        ]);
        let mut counts = BTreeMap::from([("beer".into(), 0), ("craft".into(), 0)]);
        for bytes in [&legacy, &current] {
            let directory =
                load_directory(bytes.len(), |at, n| Ok(bytes[at..at + n].to_vec())).unwrap();
            physical_counts(&directory, &mut counts).unwrap();
            accumulate_ranges(
                &directory,
                |at, n| Ok(bytes[at..at + n].to_vec()),
                &mut terms,
            )
            .unwrap();
        }
        assert_eq!(counts["beer"], 2);
        assert_eq!(terms["beer"].len(), 1);
        let q = query("beer AND craft").unwrap().unwrap();
        assert_eq!(evaluate(&q, &terms).unwrap().len(), 1);
        assert_eq!(count_upper_bound(&q, &counts), 1);
        let merged = merge_payloads(&[legacy, current]).unwrap();
        assert_eq!(word(&merged, 8), 2);
        assert_eq!(candidates(&[merged], "beer AND craft").len(), 1);
    }

    fn tid(n: usize) -> Ctid {
        let per_page = max_heap_offset();
        Ctid::new((n / per_page) as u32, (n % per_page + 1) as u16).unwrap()
    }

    fn payload(document: &str, tid: Ctid) -> Vec<u8> {
        let mut builder = Builder::default();
        builder.add(document, tid).unwrap();
        builder.finish().unwrap()
    }

    fn legacy_payload(document: &str, tid: Ctid) -> Vec<u8> {
        let current = payload(document, tid);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&word(&current, 12).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        visit_frames(&current, |term, frame| {
            bytes.extend_from_slice(&(term.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            bytes.extend_from_slice(term.as_bytes());
            bytes.extend_from_slice(frame);
            Ok(())
        })
        .unwrap();
        let total = (bytes.len() + 4) as u32;
        bytes[16..20].copy_from_slice(&total.to_le_bytes());
        let crc = crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    }

    fn candidates(payloads: &[Vec<u8>], text: &str) -> Postings {
        let q = query(text).unwrap().unwrap();
        let mut terms = BTreeMap::new();
        collect_terms(&q, &mut terms);
        for payload in payloads {
            accumulate(payload, &mut terms).unwrap();
        }
        evaluate(&q, &terms).unwrap()
    }

    fn one_term_builder(count: usize) -> Builder {
        Builder {
            terms: BTreeMap::from([("beer".into(), (0..count).map(tid).collect())]),
            logical_bytes: 4 + count * std::mem::size_of::<Ctid>(),
            pairs: count,
            wire_bytes: 16
                + "beer".len()
                + (0..count)
                    .map(|n| posting_wire_growth(n.checked_sub(1).map(tid), tid(n)))
                    .sum::<usize>(),
        }
    }

    #[test]
    fn prepared_default_analysis_is_sorted_unique_and_repeat_safe() {
        let document = prepare("WINE beer BEÉR beer craft", tid(0)).unwrap();
        assert_eq!(document.terms, ["beer", "craft", "wine"]);
        let mut builder = Builder::default();
        assert!(builder.is_empty());
        assert!(!builder.should_flush());
        builder.add_prepared(&prepare("", tid(0)).unwrap()).unwrap();
        assert!(builder.is_empty());
        assert!(builder.can_accept(&document));
        builder.add_prepared(&document).unwrap();
        let bytes = builder.logical_bytes;
        assert_eq!(builder.pairs, 3);
        builder.add_prepared(&document).unwrap();
        assert_eq!((builder.pairs, builder.logical_bytes), (3, bytes));
        // Existing term names are not charged twice, only new CTIDs are.
        builder.add("wine beer craft", tid(1)).unwrap();
        assert_eq!(
            builder.logical_bytes,
            bytes + 3 * std::mem::size_of::<Ctid>()
        );
        builder.add_prepared(&document).unwrap();
        assert_eq!(builder.pairs, 9); // nonconsecutive duplicates follow upstream
        let encoded = builder.finish().unwrap();
        assert_eq!(candidates(&[encoded], "beer AND craft").len(), 2);
    }

    #[test]
    fn admission_is_atomic_and_prepared_document_retries_after_flush() {
        let mut builder = one_term_builder(MAX_PAIRS - 1);
        let document = prepare("apple beer", tid(MAX_PAIRS)).unwrap();
        let before = builder.terms.clone();
        let counters = (builder.pairs, builder.logical_bytes, builder.wire_bytes);
        assert!(!builder.can_accept(&document));
        assert!(builder.add_prepared(&document).is_err());
        assert_eq!(builder.terms, before);
        assert_eq!(
            (builder.pairs, builder.logical_bytes, builder.wire_bytes),
            counters
        );
        assert!(builder.add("apple beer", tid(MAX_PAIRS)).is_err());
        assert_eq!(builder.terms, before);
        assert_eq!(
            (builder.pairs, builder.logical_bytes, builder.wire_bytes),
            counters
        );
        let first = std::mem::take(&mut builder).finish().unwrap();
        assert!(builder.is_empty());
        assert!(builder.can_accept(&document));
        builder.add_prepared(&document).unwrap();
        let second = builder.finish().unwrap();
        assert_eq!(
            candidates(std::slice::from_ref(&second), "apple AND beer"),
            Postings::from_ctids([tid(MAX_PAIRS)])
        );
        // Query materialization remains independently bounded; query one term
        // at a time when the overall corpus is above the old single-builder cap.
        assert_eq!(
            candidates(&[first.clone(), second.clone()], "beer").len(),
            MAX_PAIRS
        );
        assert_eq!(candidates(&[first, second], "apple").len(), 1);
    }

    #[test]
    fn preparation_failures_do_not_change_builder() {
        let mut builder = Builder::default();
        builder.add("beer", tid(0)).unwrap();
        let before = builder.terms.clone();
        let counters = (builder.pairs, builder.logical_bytes, builder.wire_bytes);
        assert!(builder.add(&"a".repeat(MAX_BYTES + 1), tid(1)).is_err());
        let unique = (0..=MAX_PAIRS)
            .map(|n| format!("word{n} "))
            .collect::<String>();
        assert!(unique.len() < MAX_BYTES);
        assert!(
            builder
                .add(&unique, tid(1))
                .unwrap_err()
                .contains("prepared document")
        );
        // Valid text length and pair count, but unique term bytes + CTIDs
        // exceed the preparation logical cap. Every token stays below 256B.
        let prefix = "a".repeat(240);
        let long_unique = (0..66_100)
            .map(|n| format!("{prefix}{n:06} "))
            .collect::<String>();
        assert!(long_unique.len() < MAX_BYTES);
        assert!(
            builder
                .add(&long_unique, tid(1))
                .unwrap_err()
                .contains("prepared document")
        );
        assert_eq!(builder.terms, before);
        assert_eq!(
            (builder.pairs, builder.logical_bytes, builder.wire_bytes),
            counters
        );
    }

    #[test]
    fn exact_hard_caps_existing_strings_and_last_tid_accounting() {
        let mut builder = one_term_builder(MAX_PAIRS);
        let repeated = prepare("beer beer", tid(MAX_PAIRS - 1)).unwrap();
        assert!(builder.can_accept(&repeated));
        builder.add_prepared(&repeated).unwrap();
        assert_eq!(builder.pairs, MAX_PAIRS);
        assert!(!builder.can_accept(&prepare("beer", tid(0)).unwrap()));
        assert!(!builder.can_accept(&prepare("apple", tid(MAX_PAIRS - 1)).unwrap()));
        // White-box byte boundary avoids millions of irrelevant token copies.
        let mut builder = one_term_builder(1);
        builder.logical_bytes = MAX_BYTES - std::mem::size_of::<Ctid>();
        let existing = prepare("beer", tid(1)).unwrap();
        assert!(builder.can_accept(&existing));
        builder.add_prepared(&existing).unwrap();
        assert_eq!(builder.logical_bytes, MAX_BYTES);
        assert!(builder.can_accept(&existing));
        let before = builder.terms.clone();
        assert!(builder.add("apple beer", tid(2)).is_err());
        assert_eq!(builder.terms, before);
        assert_eq!(builder.logical_bytes, MAX_BYTES);
        // First term fits exactly, but the second existing term needs another
        // CTID. A token-at-a-time implementation would leave "apple" behind.
        let before_bytes = MAX_BYTES - "apple".len() - std::mem::size_of::<Ctid>();
        builder.logical_bytes = before_bytes;
        assert!(builder.add("apple beer", tid(2)).is_err());
        assert_eq!(builder.terms, before);
        assert_eq!(builder.logical_bytes, before_bytes);
        assert_eq!(builder.pairs, 2);
    }

    #[test]
    fn soft_flush_uses_either_inclusive_threshold() {
        let mut builder = one_term_builder(SOFT_PAIRS - 1);
        assert!(!builder.should_flush());
        builder.add("beer", tid(SOFT_PAIRS)).unwrap();
        assert!(builder.should_flush());
        assert!(builder.can_accept(&prepare("beer", tid(SOFT_PAIRS + 1)).unwrap()));
        let mut builder = one_term_builder(1);
        builder.logical_bytes = SOFT_BYTES - std::mem::size_of::<Ctid>();
        assert!(!builder.should_flush());
        builder.add("beer", tid(1)).unwrap();
        assert_eq!(builder.logical_bytes, SOFT_BYTES);
        assert!(builder.should_flush());
        assert!(builder.pairs < SOFT_PAIRS);
    }

    #[test]
    fn incoming_document_crossing_soft_target_requests_preflush() {
        let builder = one_term_builder(SOFT_PAIRS - 1);
        let document = prepare("apple beer", tid(SOFT_PAIRS)).unwrap();
        assert!(!builder.should_flush());
        assert!(builder.projection(&document).is_some()); // hard bounds fit
        assert!(!builder.can_accept(&document));
        assert!(Builder::default().can_accept(&document));

        let mut builder = one_term_builder(1);
        builder.logical_bytes = SOFT_BYTES - std::mem::size_of::<Ctid>();
        assert!(!builder.should_flush());
        assert!(!builder.can_accept(&document));
    }

    #[test]
    fn disjoint_documents_flush_before_combined_wire_overflow() {
        let text = |start, end| {
            (start..end)
                .map(|n| format!("word{n:06} "))
                .collect::<String>()
        };
        let first = prepare(&text(0, 60_000), tid(0)).unwrap();
        let second = prepare(&text(60_000, 180_000), tid(1)).unwrap();
        assert_eq!(first.terms.len(), 60_000);
        assert_eq!(second.terms.len(), 120_000);
        let mut builder = Builder::default();
        builder.add_prepared(&first).unwrap();
        assert!(!builder.should_flush());
        assert_eq!(builder.wire_bytes + 36, 6_360_036);
        let before = builder.terms.clone();
        let counters = (builder.pairs, builder.logical_bytes, builder.wire_bytes);
        assert!(!builder.can_accept(&second));
        assert!(builder.projection(&second).is_none()); // wire hard cap, not soft hint
        assert!(builder.add_prepared(&second).is_err());
        assert_eq!(builder.terms, before);
        assert_eq!(
            (builder.pairs, builder.logical_bytes, builder.wire_bytes),
            counters
        );
        assert_eq!(
            std::mem::take(&mut builder).finish().unwrap().len(),
            6_360_036
        );
        assert!(builder.can_accept(&second));
        builder.add_prepared(&second).unwrap();
        assert_eq!(builder.finish().unwrap().len(), 12_720_036);

        // Even in an empty builder a logically fitting but unencodable whole
        // document is rejected before retaining its first term.
        let too_large = prepare(&text(0, 180_000), tid(0)).unwrap();
        let mut builder = Builder::default();
        assert!(!builder.can_accept(&too_large));
        assert!(builder.add_prepared(&too_large).is_err());
        assert!(builder.terms.is_empty());
        assert_eq!(
            (builder.pairs, builder.logical_bytes, builder.wire_bytes),
            (0, 0, 0)
        );
    }

    #[test]
    fn wire_admission_is_inclusive_atomic_and_duplicate_safe() {
        let mut builder = one_term_builder(1);
        builder.wire_bytes = MAX_BYTES - 36 - 2;
        let next = prepare("beer", tid(1)).unwrap();
        assert!(builder.can_accept(&next));
        builder.add_prepared(&next).unwrap();
        assert_eq!(builder.wire_bytes + 36, MAX_BYTES);
        assert!(builder.can_accept(&next));
        builder.add_prepared(&next).unwrap(); // consecutive duplicate costs zero
        let before = builder.terms.clone();
        let counters = (builder.pairs, builder.logical_bytes, builder.wire_bytes);
        let overflow = prepare("apple beer", tid(2)).unwrap();
        assert!(!builder.can_accept(&overflow));
        assert!(builder.add_prepared(&overflow).is_err());
        assert_eq!(builder.terms, before);
        assert_eq!(
            (builder.pairs, builder.logical_bytes, builder.wire_bytes),
            counters
        );
    }

    #[test]
    fn wire_run_bound_covers_pages_groups_revisits_and_duplicate_offsets() {
        let cases = [
            vec![(0, 1), (0, 2), (1, 1), (255, 1), (256, 1), (512, 1)],
            vec![(512, 1), (0, 1), (256, 1), (0, 2), (512, 1), (512, 1)],
            vec![(0, 2), (0, 1), (0, 2), (0, 1)],
            vec![(u32::MAX - 1, u16::MAX)],
        ];
        for (n, ctids) in cases.into_iter().enumerate() {
            let mut builder = Builder::default();
            for (block, offset) in ctids {
                builder
                    .add("beer craft", Ctid::new(block, offset).unwrap())
                    .unwrap();
            }
            let bound = builder.wire_bytes + 36;
            let actual = builder.finish().unwrap().len();
            assert!(actual <= bound, "case {n}: actual {actual}, bound {bound}");
            if n == 0 || n == 3 {
                assert_eq!(actual, bound);
            }
        }
    }

    #[test]
    fn common_vocabulary_200k_documents_keeps_dense_batching() {
        let mut builder = Builder::default();
        let mut document = prepare("beer craft", tid(0)).unwrap();
        let mut batches = 0;
        let mut encoded_bytes = 0;
        for n in 0..200_000 {
            document.tid = tid(n);
            if !builder.is_empty() && (builder.should_flush() || !builder.can_accept(&document)) {
                let bound = builder.wire_bytes + 36;
                let bytes = std::mem::take(&mut builder).finish().unwrap();
                assert_eq!(bytes.len(), bound);
                encoded_bytes += bytes.len();
                batches += 1;
            }
            builder.add_prepared(&document).unwrap();
        }
        let bound = builder.wire_bytes + 36;
        let bytes = builder.finish().unwrap();
        assert_eq!(bytes.len(), bound);
        encoded_bytes += bytes.len();
        batches += 1;
        assert_eq!(batches, 7); // soft pair target, not pessimistic singleton costs
        assert!(encoded_bytes < 1_000_000);
    }

    #[test]
    fn multi_batch_corpus_exceeds_old_cap_without_losing_candidates() {
        let mut builder = Builder::default();
        let mut segments = Vec::new();
        let count = MAX_PAIRS / 4 + 1;
        for n in 0..count {
            let document = prepare("beer craft wine rare", tid(n)).unwrap();
            if !builder.can_accept(&document) {
                segments.push(std::mem::take(&mut builder).finish().unwrap());
            }
            builder.add_prepared(&document).unwrap();
            if builder.should_flush() {
                segments.push(std::mem::take(&mut builder).finish().unwrap());
            }
        }
        if !builder.is_empty() {
            segments.push(builder.finish().unwrap());
        }
        assert_eq!(segments.len(), 5);
        let expected = Postings::from_ctids((0..count).map(tid));
        for text in ["rare", "beer AND craft", "beer OR wine"] {
            assert_eq!(candidates(&segments, text), expected, "{text}");
        }
        assert!(candidates(&segments, "absent").is_empty());
    }

    #[test]
    fn merge_unions_duplicate_and_split_ctids_before_and_canonically() {
        let segments = vec![
            payload("beer", tid(0)),
            payload("craft", tid(0)),
            payload("beer beer", tid(0)),
            payload("craft wine", tid(1)),
            payload("beer", tid(1)),
        ];
        let merged = merge_payloads(&segments).unwrap();
        let mut direct = Builder::default();
        direct.add("beer craft", tid(0)).unwrap();
        direct.add("beer craft wine", tid(1)).unwrap();
        assert_eq!(merged, direct.finish().unwrap());
        for text in ["beer", "beer AND craft", "wine OR craft", "absent"] {
            assert_eq!(
                candidates(&segments, text),
                candidates(std::slice::from_ref(&merged), text)
            );
        }
        let mut reverse = segments.clone();
        reverse.reverse();
        assert_eq!(merge_payloads(&reverse).unwrap(), merged);
        assert_eq!(
            merge_payloads(std::slice::from_ref(&merged)).unwrap(),
            merged
        );
        assert!(merge_payloads(&[]).unwrap().is_empty());
        assert!(merge_payloads(&[Vec::new()]).unwrap().is_empty());
    }

    #[test]
    fn merge_validates_skipped_codec_frames_not_just_envelope_crc() {
        let mut bad = legacy_payload("beer", tid(0));
        let frame_start = 28 + 4; // dictionary header and "beer"
        bad[frame_start + 20] = 1; // codec reserved field, not canonical
        let end = bad.len() - 4;
        let frame_crc = crc32c(
            &[
                &bad[frame_start..frame_start + 36],
                &bad[frame_start + 40..end],
            ]
            .concat(),
        );
        bad[frame_start + 36..frame_start + 40].copy_from_slice(&frame_crc.to_le_bytes());
        let crc = crc32c(&bad[..end]);
        bad[end..].copy_from_slice(&crc.to_le_bytes());
        // Query accumulation deliberately skips frames for absent terms.
        accumulate(&bad, &mut BTreeMap::new()).unwrap();
        assert!(
            merge_payloads(&[payload("craft", tid(1)), bad])
                .unwrap_err()
                .contains("reserved")
        );
    }

    #[test]
    fn merge_rejects_resealed_codec_valid_impossible_offsets_in_unqueried_terms() {
        let maximum = u16::try_from(max_heap_offset()).unwrap();
        let boundary = legacy_payload("beer", Ctid::new(0, maximum).unwrap());
        let other = payload("craft", tid(0));
        assert!(merge_payloads(&[boundary.clone(), other.clone()]).is_ok());
        for impossible in [maximum + 1, u16::MAX] {
            let mut bad = boundary.clone();
            let frame_start = 28 + "beer".len();
            let end = bad.len() - 4;
            // Singleton frame: 40-byte header, 36-byte group, 2-byte page count.
            bad[frame_start + 78..frame_start + 80].copy_from_slice(&impossible.to_le_bytes());
            let frame_crc = crc32c(
                &[
                    &bad[frame_start..frame_start + 36],
                    &bad[frame_start + 40..end],
                ]
                .concat(),
            );
            bad[frame_start + 36..frame_start + 40].copy_from_slice(&frame_crc.to_le_bytes());
            let envelope_crc = crc32c(&bad[..end]);
            bad[end..].copy_from_slice(&envelope_crc.to_le_bytes());
            let decoded = decode_frame(&bad[frame_start..end], MAX_PAIRS).unwrap();
            assert_eq!(decoded.iter().next().unwrap().offset(), impossible);
            let mut queried = BTreeMap::from([("craft".into(), Postings::default())]);
            accumulate(&bad, &mut queried).unwrap(); // beer is not queried
            let inputs = vec![other.clone(), bad];
            let before = inputs.clone();
            assert!(
                merge_payloads(&inputs)
                    .unwrap_err()
                    .contains("impossible heap tuple offset")
            );
            assert_eq!(inputs, before);
        }
    }

    #[test]
    fn merge_enforces_cumulative_decoded_pair_and_input_limits() {
        let half = one_term_builder(MAX_PAIRS / 2).finish().unwrap();
        // Duplicates union away, but still consume cumulative decode budget.
        let merged = merge_payloads(&[half.clone(), half.clone()]).unwrap();
        assert_eq!(candidates(&[merged], "beer").len(), MAX_PAIRS / 2);
        assert!(merge_payloads(&[half.clone(), half, payload("craft", tid(0))]).is_err());
        assert!(
            merge_payloads(&[vec![0; MAX_BYTES], vec![0]])
                .unwrap_err()
                .contains("merge input")
        );
    }

    #[test]
    fn finish_rejects_encoded_overhead_beyond_payload_cap() {
        // <=262144 pairs and <16 MiB logical bytes can still exceed 16 MiB
        // on wire because every distinct term carries its own codec frame.
        let terms: BTreeMap<_, _> = (0..190_000)
            .map(|n| (format!("word{n:06}"), vec![tid(0)]))
            .collect();
        let builder = Builder {
            logical_bytes: terms
                .keys()
                .map(|s| s.len() + std::mem::size_of::<Ctid>())
                .sum(),
            pairs: terms.len(),
            terms,
            ..Builder::default()
        };
        assert!(builder.logical_bytes < MAX_BYTES);
        assert!(
            builder
                .finish()
                .unwrap_err()
                .contains("encoded term segment")
        );
    }
    #[test]
    fn dictionary_checksum_covers_term_names() {
        let mut b = Builder::default();
        b.add("beer", Ctid::new(256, 1).unwrap()).unwrap();
        let mut bytes = b.finish().unwrap();
        bytes[V2_HEADER + 16] ^= 1;
        assert!(
            accumulate(&bytes, &mut BTreeMap::new())
                .unwrap_err()
                .contains("CRC32C")
        );
        assert_eq!(crc32c(b"123456789"), 0xe3069283);
    }
    #[test]
    fn union_each_term_before_boolean() {
        let tid = Ctid::new(512, 2).unwrap();
        let q = query("beer AND craft").unwrap().unwrap();
        let mut terms = BTreeMap::new();
        collect_terms(&q, &mut terms);
        for term in ["beer", "craft"] {
            let mut b = Builder::default();
            b.add(term, tid).unwrap();
            accumulate(&b.finish().unwrap(), &mut terms).unwrap();
        }
        assert_eq!(
            evaluate(&q, &terms).unwrap().iter().collect::<Vec<_>>(),
            vec![tid]
        );
        assert!(query("beer AND NOT craft").unwrap().is_none());
        assert!(query("\"beer craft\"").unwrap().is_none());
    }

    #[test]
    fn indexed_candidates_match_default_runtime() {
        let mut documents = Vec::new();
        let mut segments = Vec::new();
        for n in 0..128u32 {
            let tid = Ctid::new(
                [0, 255, 256, 511, 512, 1024][n as usize % 6],
                (n + 1) as u16,
            )
            .unwrap();
            let text = format!(
                "{} {} {} {}",
                if n % 2 == 0 { "BEÉR" } else { "wine" },
                if n % 3 == 0 { "craft" } else { "festival" },
                if n % 7 == 0 { "rare" } else { "" },
                if n % 11 == 0 { "🍺" } else { "" }
            );
            let mut b = Builder::default();
            b.add(&text, tid).unwrap();
            segments.push(b.finish().unwrap());
            documents.push((tid, text));
        }
        for text in [
            "beer",
            "craft AND beer",
            "beer OR wine",
            "(beer AND craft) OR (wine AND rare)",
            "beer^2",
            "absent",
            "🍺",
        ] {
            let q = query(text).unwrap().unwrap();
            let mut terms = BTreeMap::new();
            collect_terms(&q, &mut terms);
            for segment in &segments {
                accumulate(segment, &mut terms).unwrap();
            }
            let expected = Postings::from_ctids(documents.iter().filter_map(|(tid, text)| {
                let doc = tinql::runtime::tokenize_doc(text, default_pipeline());
                tinql::runtime::evaluate(&q, &doc)
                    .unwrap()
                    .matched
                    .then_some(*tid)
            }));
            assert_eq!(evaluate(&q, &terms).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn full_memory_validation_detects_corruption_even_without_terms() {
        let mut b = Builder::default();
        b.add("beer craft wine", Ctid::new(255, 7).unwrap())
            .unwrap();
        let bytes = b.finish().unwrap();
        for n in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[n] ^= 0x80;
            assert!(accumulate(&bad, &mut BTreeMap::new()).is_err(), "byte {n}");
            assert!(merge_payloads(&[bad]).is_err(), "merge byte {n}");
        }
        for n in 1..bytes.len() {
            assert!(accumulate(&bytes[..n], &mut BTreeMap::new()).is_err());
            assert!(merge_payloads(&[bytes[..n].to_vec()]).is_err());
        }
    }

    #[test]
    fn rejects_query_and_builder_limits() {
        assert!(query(&"x".repeat(MAX_QUERY_BYTES + 1)).is_err());
        assert!(query(&format!("{}beer{}", "(".repeat(65), ")".repeat(65))).is_err());
        let mut b = Builder {
            pairs: MAX_PAIRS,
            ..Builder::default()
        };
        assert!(
            b.add("beer", Ctid::new(0, 1).unwrap())
                .unwrap_err()
                .contains("builder limit")
        );
        assert!(query("bee*").unwrap().is_none());
    }
}
