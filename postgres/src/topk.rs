// Copyright (C) 2026 Plumb contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exact, bounded, caller-visible heap BM25. This is NOT WAND or index-backed
//! scoring: every visible row is visited once, and matching compact features are
//! retained until corpus statistics are known. Only the final k scores are kept.

use crate::bm25::{TermScorer, TermSetEdit, compile_scoring_terms, sum_scores_in_order};
use pgrx::{PgRelation, Spi, default, name, pg_extern, pg_sys};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ffi::{CStr, CString};
use tinql::runtime::{Query, TokenizedDoc, evaluate, parse_tinql_to_query};
use tokenizer::Tokenizer;

const MAX_QUERY_BYTES: usize = 4096;
const MAX_TERMS: usize = 64;
const MAX_QUERY_NODES: usize = 256;
const MAX_DEPTH: usize = 32;
const MAX_ROWS: usize = 1_000_000;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const MAX_CORPUS_BYTES: usize = 256 * 1024 * 1024;
const MAX_DOCUMENT_TOKENS: usize = 32_768;
const MAX_CANDIDATES: usize = 100_000;
const MAX_FEATURE_BYTES: usize = 32 * 1024 * 1024;
const FETCH_ROWS: i64 = 32;

type ResultRow = (pg_sys::ItemPointerData, f32);

struct Candidate {
    ctid: pg_sys::ItemPointerData,
    length: u32,
    tf: Box<[u32]>,
}

#[derive(Clone, Copy)]
struct Ranked {
    ctid: pg_sys::ItemPointerData,
    score: f32,
}

fn tid_key(tid: &pg_sys::ItemPointerData) -> (u32, u16) {
    (
        (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
        tid.ip_posid,
    )
}

// BinaryHeap's greatest element is the WORST retained row: smaller score, or
// larger CTID on a tie. into_sorted_vec therefore returns the required order.
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| tid_key(&self.ctid).cmp(&tid_key(&other.ctid)))
    }
}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked {}

fn validate_query(query: &Query, depth: usize, nodes: &mut usize) {
    *nodes += 1;
    if depth > MAX_DEPTH || *nodes > MAX_QUERY_NODES {
        pgrx::error!("plumb.top_k query exceeds depth/node budget");
    }
    match query {
        Query::Term(_) => {}
        Query::And(a, b) | Query::Or(a, b) => {
            validate_query(a, depth + 1, nodes);
            validate_query(b, depth + 1, nodes);
        }
        Query::Conjunction(children) | Query::Disjunction { min: 1, children } => {
            for child in children {
                validate_query(child, depth + 1, nodes);
            }
        }
        Query::Boost { factor, inner } if factor.is_finite() && *factor >= 0.0 => {
            validate_query(inner, depth + 1, nodes);
        }
        _ => pgrx::error!(
            "plumb.top_k supports only positive term, AND, OR and finite nonnegative boost queries"
        ),
    }
}

/// Raw regclass OID avoids PgRelation conversion locking the index before the
/// heap. SQL is deliberately SECURITY INVOKER. The raw read-only SPI cursor
/// below reuses the caller's active statement snapshot even if a surrounding
/// pgrx SPI session has previously marked itself mutable.
#[pg_extern(sql = "
    CREATE FUNCTION @extschema@.top_k(index regclass, query text, k integer DEFAULT 10)
        RETURNS TABLE(ctid tid, score real)
        STABLE STRICT PARALLEL UNSAFE
        LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
")]
fn top_k(
    index: pg_sys::Oid,
    query: &str,
    k: default!(i32, 10),
) -> pgrx::iter::TableIterator<'static, (name!(ctid, pg_sys::ItemPointerData), name!(score, f32))> {
    if !(0..=1000).contains(&k) {
        pgrx::error!("plumb.top_k k must be between 0 and 1000");
    }
    if query.len() > MAX_QUERY_BYTES {
        pgrx::error!("plumb.top_k query exceeds 4096-byte budget");
    }
    // Conservative pre-parser guard, including quoted/escaped parentheses. An
    // over-complex spelling may be rejected; it is never silently reinterpreted.
    if query
        .bytes()
        .filter(|b| matches!(b, b'(' | b'[' | b'{'))
        .count()
        > MAX_DEPTH
        || query.split_whitespace().count() > MAX_QUERY_NODES
    {
        pgrx::error!("plumb.top_k query exceeds depth/node budget");
    }
    let rows = unsafe { run_top_k(index, query, k as usize) };
    pgrx::iter::TableIterator::new(rows)
}

unsafe fn run_top_k(index_oid: pg_sys::Oid, query_text: &str, k: usize) -> Vec<ResultRow> {
    unsafe {
        let heap_oid = pg_sys::IndexGetRelation(index_oid, true);
        if heap_oid == pg_sys::InvalidOid {
            pgrx::error!("plumb.top_k requires a valid plumb index");
        }
        let heap = PgRelation::with_lock(heap_oid, pg_sys::AccessShareLock as _);
        let index = PgRelation::with_lock(index_oid, pg_sys::AccessShareLock as _);
        let relation = index.as_ptr();
        if (*(*relation).rd_rel).relkind != pg_sys::RELKIND_INDEX as i8
            || (*(*relation).rd_rel).relam != pg_sys::get_am_oid(c"plumb".as_ptr(), false)
            || (*relation).rd_index.is_null()
        {
            pgrx::error!("plumb.top_k requires a valid plumb index");
        }
        let definition = &*(*relation).rd_index;
        if definition.indrelid != heap_oid || definition.indexrelid != index_oid {
            pgrx::error!("plumb.top_k index identity changed while acquiring locks; retry");
        }
        if !definition.indisvalid || !definition.indisready || !definition.indislive {
            pgrx::error!("plumb.top_k requires a valid, ready, live index");
        }
        if definition.indnatts != 1 || definition.indnkeyatts != 1 {
            pgrx::error!("plumb.top_k requires a single-column index");
        }
        if (*(*heap.as_ptr()).rd_rel).relkind != pg_sys::RELKIND_RELATION as i8
            || (*(*heap.as_ptr()).rd_rel).relhassubclass
        {
            pgrx::error!("plumb.top_k does not support partitioned or inheritance-parent tables");
        }
        if !pg_sys::RelationGetIndexExpressions(relation).is_null()
            || !pg_sys::RelationGetIndexPredicate(relation).is_null()
        {
            pgrx::error!("plumb.top_k does not yet support expression or partial indexes");
        }
        let attnum = *definition.indkey.values.as_ptr();
        if attnum <= 0 || pg_sys::get_atttype(heap_oid, attnum) != pg_sys::TEXTOID {
            pgrx::error!("plumb.top_k requires a plain text column");
        }
        if crate::options::postings_requested(relation) {
            crate::options::validate_postings(relation);
        }
        let tokenizer = crate::options::tokenizer(relation);
        let params = crate::options::bm25(relation)
            .checked()
            .unwrap_or_else(|e| pgrx::error!("plumb.top_k BM25 parameters: {e}"));
        let query = parse_tinql_to_query(query_text, &tokenizer)
            .unwrap_or_else(|e| pgrx::error!("plumb.top_k query error: {e}"));
        validate_query(&query, 0, &mut 0);
        let mut inputs = Vec::new();
        crate::score::collect_score_terms(&query, 1.0, false, &mut inputs);
        // Budget the normalized scoring occurrences, before combining boosts.
        // The shared parser may deduplicate flat unboosted siblings; boosted
        // repetitions remain separate inputs and each consumes this budget.
        if inputs.len() > MAX_TERMS {
            pgrx::error!("plumb.top_k query exceeds 64-term budget");
        }
        let terms = compile_scoring_terms(inputs, &TermSetEdit::None, None);
        if terms.iter().any(|t| !t.boost().is_finite()) {
            pgrx::error!("plumb.top_k combined term boost must be finite");
        }
        let table = pg_sys::quote_qualified_identifier(
            pg_sys::get_namespace_name(pg_sys::get_rel_namespace(heap_oid)),
            pg_sys::get_rel_name(heap_oid),
        );
        let column = pg_sys::quote_identifier(pg_sys::get_attname(heap_oid, attnum, false));
        let table = CStr::from_ptr(table).to_string_lossy();
        let column = CStr::from_ptr(column).to_string_lossy();
        // All identifiers are catalog-derived and quoted, and query text NEVER
        // enters SQL. CASE prevents oversized text entering the SPI result set.
        // Include null rows in the scan budget, but not in BM25 statistics.
        let sql = CString::new(format!(
            "SELECT ctid, pg_catalog.octet_length({column}), \
             CASE WHEN pg_catalog.octet_length({column}) OPERATOR(pg_catalog.<=) {MAX_DOCUMENT_BYTES} THEN {column} ELSE NULL END \
             FROM ONLY {table} LIMIT {}", MAX_ROWS + 1
        )).expect("quoted catalog identifiers contain no NUL");
        let mut candidates = Vec::<Candidate>::new();
        let mut df = vec![0_u64; terms.len()];
        let mut documents = 0_u64;
        let mut total_length = 0_usize;
        let mut scanned_rows = 0_usize;
        let mut corpus_bytes = 0_usize;
        let mut feature_bytes = 0_usize;
        Spi::connect(|client| {
            // Execute the same OID-checked plan, not name-based SQL that could
            // resolve to a replacement after a concurrent schema rename. The
            // shared helper uses the active snapshot (SPI read_only semantics).
            let portal = crate::score::open_corpus_cursor(&sql, heap_oid);
            if portal.is_null() {
                pgrx::error!("plumb.top_k could not open corpus cursor");
            }
            let cursor_name = CStr::from_ptr((*portal).name).to_string_lossy();
            let mut cursor = client
                .find_cursor(&cursor_name)
                .expect("newly opened SPI cursor exists");
            // Opening the query performs normal invoker SELECT checks even for
            // k=0, but no document scan is needed in that case.
            if k == 0 {
                return;
            }
            loop {
                pgrx::check_for_interrupts!();
                let batch = cursor
                    .fetch(FETCH_ROWS as _)
                    .unwrap_or_else(|e| pgrx::error!("plumb.top_k corpus fetch failed: {e}"));
                let table_ptr = pg_sys::SPI_tuptable;
                let count = batch.len();
                for row in batch {
                    scanned_rows += 1;
                    if scanned_rows > MAX_ROWS {
                        pgrx::error!("plumb.top_k exceeds 1000000-row scan budget");
                    }
                    let bytes = row.get::<i32>(2).expect("document length column");
                    let Some(bytes) = bytes else {
                        continue;
                    };
                    if bytes as usize > MAX_DOCUMENT_BYTES {
                        pgrx::error!("plumb.top_k document exceeds 262144-byte budget");
                    }
                    corpus_bytes += bytes as usize;
                    if corpus_bytes > MAX_CORPUS_BYTES {
                        pgrx::error!("plumb.top_k exceeds 268435456-byte corpus budget");
                    }
                    let text = row
                        .get::<String>(3)
                        .expect("document text column")
                        .expect("non-null bounded document");
                    let mut tokens = Vec::new();
                    for token in tokenizer.tokenize(&text) {
                        if tokens.len() >= MAX_DOCUMENT_TOKENS {
                            pgrx::error!("plumb.top_k document exceeds 32768-token budget");
                        }
                        if tokens.len().is_multiple_of(1024) {
                            pgrx::check_for_interrupts!();
                        }
                        tokens.push((token.text.into_owned(), token.pos));
                    }
                    let doc = TokenizedDoc::new(tokens);
                    documents += 1;
                    total_length += doc.len();
                    let tf: Vec<u32> = terms
                        .iter()
                        .enumerate()
                        .map(|(i, term)| {
                            let count = doc.positions(term.text()).len() as u32;
                            if count > 0 {
                                df[i] += 1;
                            }
                            count
                        })
                        .collect();
                    let matched = evaluate(&query, &doc)
                        .unwrap_or_else(|e| pgrx::error!("plumb.top_k evaluation failed: {e}"))
                        .matched;
                    if matched {
                        // Charge twice the Vec element size to account for its
                        // geometric spare capacity; boxed TF arrays have no spare.
                        let cost = 2 * std::mem::size_of::<Candidate>() + 4 * tf.len();
                        if candidates.len() >= MAX_CANDIDATES
                            || feature_bytes + cost > MAX_FEATURE_BYTES
                        {
                            pgrx::error!(
                                "plumb.top_k exceeds candidate feature budget (100000 rows / 33554432 bytes)"
                            );
                        }
                        feature_bytes += cost;
                        candidates.push(Candidate {
                            ctid: row
                                .get::<pg_sys::ItemPointerData>(1)
                                .expect("ctid column")
                                .expect("non-null ctid"),
                            length: doc.len() as u32,
                            tf: tf.into_boxed_slice(),
                        });
                    }
                }
                // pgrx does not free fetch result tables on Drop. All copied
                // Rust values above are owned; no tuple/Datum survives this free.
                if !table_ptr.is_null() {
                    pg_sys::SPI_freetuptable(table_ptr);
                    pg_sys::SPI_tuptable = std::ptr::null_mut();
                }
                if count == 0 {
                    break;
                }
            }
        });
        if k == 0 {
            return Vec::new();
        }
        let average_length = if documents == 0 {
            1.0
        } else {
            total_length as f32 / documents as f32
        };
        let scorers: Vec<_> = terms
            .iter()
            .zip(df)
            .map(|(term, df)| {
                (df > 0).then(|| {
                    TermScorer::from_statistics(documents, df, term.boost(), params, average_length)
                        .unwrap_or_else(|e| pgrx::error!("plumb.top_k BM25 parameters: {e}"))
                })
            })
            .collect();
        let mut winners = BinaryHeap::<Ranked>::with_capacity(k);
        for (i, candidate) in candidates.into_iter().enumerate() {
            if i.is_multiple_of(1024) {
                pgrx::check_for_interrupts!();
            }
            // Identical lexical term order, absent-term omission and inherited
            // score_count TF bucketing as full_score; never reorder reductions.
            let score = sum_scores_in_order(scorers.iter().zip(candidate.tf.iter()).filter_map(
                |(scorer, tf)| {
                    scorer.as_ref().map(|scorer| {
                        if *tf == 0 {
                            0.0
                        } else {
                            scorer.score_count(*tf, candidate.length)
                        }
                    })
                },
            ));
            if !score.is_finite() {
                pgrx::error!("plumb.top_k score is not finite");
            }
            let row = Ranked {
                ctid: candidate.ctid,
                score,
            };
            if winners.len() < k {
                winners.push(row);
            } else if winners.peek().is_some_and(|worst| row < *worst) {
                *winners.peek_mut().expect("nonempty bounded heap") = row;
            }
        }
        winners
            .into_sorted_vec()
            .into_iter()
            .map(|row| (row.ctid, row.score))
            .collect()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn bounded_heap_matches_reference_sort_with_score_and_ctid_ties() {
        let corpus: Vec<_> = (0..2048_u32)
            .map(|i| Ranked {
                ctid: pg_sys::ItemPointerData {
                    ip_blkid: pg_sys::BlockIdData {
                        bi_hi: (i / 256) as u16,
                        bi_lo: (i % 256) as u16,
                    },
                    ip_posid: (1 + i % 31) as u16,
                },
                score: ((i.wrapping_mul(747796405).wrapping_add(2891336453) >> 24) % 13) as f32,
            })
            .collect();
        for k in [0, 1, 10, 1000] {
            let mut reference = corpus.clone();
            reference.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| tid_key(&a.ctid).cmp(&tid_key(&b.ctid)))
            });
            reference.truncate(k);
            let mut heap = BinaryHeap::with_capacity(k);
            if k > 0 {
                for row in &corpus {
                    if heap.len() < k {
                        heap.push(*row);
                    } else if heap.peek().is_some_and(|worst| row < worst) {
                        *heap.peek_mut().unwrap() = *row;
                    }
                    assert!(heap.len() <= k);
                }
            }
            let actual = heap.into_sorted_vec();
            let keys = |rows: Vec<Ranked>| {
                rows.into_iter()
                    .map(|r| (tid_key(&r.ctid), r.score.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(keys(actual), keys(reference));
        }
    }
}
