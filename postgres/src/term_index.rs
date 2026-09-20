// SPDX-License-Identifier: AGPL-3.0-or-later
// Added by Plumb contributors on 2026-09-20: experimental bounded term segments.
//! Immutable term dictionary + scalar grouped-CTID frames. No visibility authority.
use plumb_postings::{Ctid, Postings, codec};
use std::collections::BTreeMap;
use tinql::runtime::Query;
use tokenizer::{Tokenizer, presets::default_pipeline};

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PAIRS: usize = 256 * 1024;
pub const MAX_QUERY_BYTES: usize = 1024;
const MAX_NODES: usize = 256;
const MAGIC: &[u8; 8] = b"PLMBTRM\0";

// Host-side scalar unit tests do not have a PostgreSQL interrupt context.
// The extension (including pg_test SQL builds) always retains these checks.
#[inline]
fn check_interrupts() {
    #[cfg(not(test))]
    pgrx::check_for_interrupts!();
}

#[derive(Default)]
pub struct Builder {
    terms: BTreeMap<String, Vec<Ctid>>,
    logical_bytes: usize,
    pairs: usize,
}

impl Builder {
    pub fn add(&mut self, document: &str, tid: Ctid) -> Result<(), String> {
        if document.len() > MAX_BYTES {
            return Err("postings_v1 document exceeds 16 MiB text limit".into());
        }
        for (n, token) in default_pipeline().tokenize(document).enumerate() {
            if n % 1024 == 0 {
                check_interrupts();
            }
            let term = token.text.as_ref();
            if self.terms.get(term).is_some_and(|v| v.last() == Some(&tid)) {
                continue;
            }
            let additional = std::mem::size_of::<Ctid>()
                + if self.terms.contains_key(term) {
                    0
                } else {
                    term.len()
                };
            if self.pairs >= MAX_PAIRS || self.logical_bytes + additional > MAX_BYTES {
                return Err("postings_v1 builder limit exceeded (16 MiB logical term strings/CTIDs or 262144 term/CTID pairs); reduce corpus; work_mem spilling is not implemented".into());
            }
            self.logical_bytes += additional;
            self.pairs += 1;
            self.terms.entry(term.to_owned()).or_default().push(tid);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>, String> {
        if self.terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(self.terms.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for (term, tids) in self.terms {
            check_interrupts();
            let frame = codec::encode(&Postings::from_ctids(tids)).map_err(|e| e.to_string())?;
            if bytes.len() + 8 + term.len() + frame.len() + 4 > MAX_BYTES {
                return Err("postings_v1 encoded term segment exceeds 16 MiB".into());
            }
            bytes.extend_from_slice(&(term.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            bytes.extend_from_slice(term.as_bytes());
            bytes.extend_from_slice(&frame);
        }
        let len = (bytes.len() + 4) as u32;
        bytes[16..20].copy_from_slice(&len.to_le_bytes());
        let crc = crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        Ok(bytes)
    }
}

// Castagnoli polynomial. Covers header, dictionary names, lengths AND all frames.
// The trailing CRC field itself is excluded, as in conventional CRC envelopes.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for (position, &byte) in bytes.iter().enumerate() {
        if position % 65536 == 0 {
            check_interrupts();
        }
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
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

/// Validate the complete dictionary even when no query term occurs in it.
/// Decode only matching frames; the envelope CRC covers skipped frames as well.
pub fn accumulate(bytes: &[u8], terms: &mut BTreeMap<String, Postings>) -> Result<(), String> {
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
        let frame = take(&mut data, frame_len)?;
        if let Some(existing) = terms.get_mut(term) {
            let decoded = codec::decode(
                frame,
                codec::DecodeLimits {
                    max_encoded_bytes: MAX_BYTES,
                    max_postings: MAX_PAIRS,
                    max_groups: MAX_PAIRS,
                    max_pages: MAX_PAIRS,
                    max_decoded_bytes: 64 * 1024 * 1024,
                },
            )
            .map_err(|e| format!("postings_v1 frame: {e}"))?;
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
    }
    if !data.is_empty() {
        return Err("trailing postings_v1 term segment bytes".into());
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dictionary_checksum_covers_term_names() {
        let mut b = Builder::default();
        b.add("beer", Ctid::new(256, 1).unwrap()).unwrap();
        let mut bytes = b.finish().unwrap();
        bytes[28] ^= 1;
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
    fn corruption_is_detected_even_for_absent_query_terms() {
        let mut b = Builder::default();
        b.add("beer craft wine", Ctid::new(255, 7).unwrap())
            .unwrap();
        let bytes = b.finish().unwrap();
        for n in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[n] ^= 0x80;
            assert!(accumulate(&bad, &mut BTreeMap::new()).is_err(), "byte {n}");
        }
        for n in 1..bytes.len() {
            assert!(accumulate(&bytes[..n], &mut BTreeMap::new()).is_err());
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
