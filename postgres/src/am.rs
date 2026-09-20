// Copyright (C) 2026 PlanetScale
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.
//
// The full license text is available in LICENSE.
// Modified by Plumb contributors on 2026-09-20: experimental persisted CTID bitmap index path.
use pgrx::{PgBox, pg_extern, pg_guard, pg_sys};
use std::ffi::c_void;

#[pg_extern(sql = "
    CREATE FUNCTION @extschema@.amhandler(internal)
        RETURNS index_am_handler
        PARALLEL SAFE IMMUTABLE STRICT
        LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
    CREATE ACCESS METHOD plumb TYPE INDEX HANDLER @extschema@.amhandler;
")]
pub(crate) fn amhandler(_fcinfo: pg_sys::FunctionCallInfo) -> PgBox<pg_sys::IndexAmRoutine> {
    let mut routine =
        unsafe { PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine) };
    routine.amstrategies = 1;
    routine.amsupport = 0;
    routine.amcanmulticol = false;
    routine.amsearcharray = false;
    routine.amkeytype = pg_sys::InvalidOid;
    routine.amvalidate = Some(amvalidate);
    routine.ambuild = Some(ambuild);
    routine.ambuildempty = Some(ambuildempty);
    routine.aminsert = Some(aminsert);
    routine.ambulkdelete = Some(ambulkdelete);
    routine.amvacuumcleanup = Some(amvacuumcleanup);
    routine.amcostestimate = Some(amcostestimate);
    routine.amoptions = Some(crate::options::amoptions);
    routine.ambeginscan = Some(ambeginscan);
    routine.amrescan = Some(amrescan);
    routine.amgetbitmap = Some(amgetbitmap);
    routine.amendscan = Some(amendscan);
    routine.into_pg_boxed()
}

/// Only the canonical text-search operator can be evaluated by the postings
/// engine. Resolve through PostgreSQL's catalogs, never search_path or an OID
/// cached across DDL. An alternate opclass is fine only with identical semantics.
unsafe fn postings_operator(opfamily: pg_sys::Oid) -> Option<pg_sys::Oid> {
    unsafe {
        let canonical = crate::operator::search_operator_oid();
        if canonical == pg_sys::InvalidOid
            || pg_sys::get_opfamily_member(opfamily, pg_sys::TEXTOID, pg_sys::TEXTOID, 1)
                != canonical
        {
            return None;
        }
        let function = pg_sys::get_opcode(canonical);
        if function == pg_sys::InvalidOid
            || pg_sys::get_op_rettype(canonical) != pg_sys::BOOLOID
            || !pg_sys::func_strict(function)
            || pg_sys::func_volatile(function) != pg_sys::PROVOLATILE_IMMUTABLE as i8
        {
            return None;
        }
        Some(function)
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn amvalidate(opclassoid: pg_sys::Oid) -> bool {
    unsafe {
        let tuple =
            pg_sys::SearchSysCache1(pg_sys::SysCacheIdentifier::CLAOID as i32, opclassoid.into());
        if tuple.is_null() {
            return false;
        }
        let opclass = *pg_sys::GETSTRUCT(tuple).cast::<pg_sys::FormData_pg_opclass>();
        pg_sys::ReleaseSysCache(tuple);
        opclass.opcintype == pg_sys::TEXTOID
            && (opclass.opckeytype == pg_sys::InvalidOid || opclass.opckeytype == pg_sys::TEXTOID)
            && postings_operator(opclass.opcfamily).is_some()
    }
}

/// Validate before touching any indexed Datum, including an empty build. Repeat
/// at insert/scan so indexes made by older versions or changed catalogs fail
/// closed. The heap baseline deliberately does not use these restrictions.
unsafe fn validate_postings_semantics(index: pg_sys::Relation) -> pg_sys::Oid {
    unsafe {
        // PgTupleDesc handles the different PG17/PG18 attribute layouts without
        // taking ownership or changing the relation descriptor's refcount.
        let descriptor = pgrx::PgTupleDesc::from_pg_unchecked((*index).rd_att);
        if (*(*index).rd_index).indnkeyatts != 1
            || descriptor.len() != 1
            || (*index).rd_opcintype.is_null()
            || *(*index).rd_opcintype != pg_sys::TEXTOID
            || descriptor.get(0).map(|attr| attr.atttypid) != Some(pg_sys::TEXTOID)
        {
            pgrx::error!("postings_v1 requires a text input and text index key");
        }
        if (*index).rd_opfamily.is_null() {
            pgrx::error!("postings_v1 requires strategy 1 to be pg_catalog.~~>(text,text)");
        }
        let function = postings_operator(*(*index).rd_opfamily).unwrap_or_else(|| {
            pgrx::error!("postings_v1 requires strategy 1 to be pg_catalog.~~>(text,text)")
        });
        // The canonical analyzer is collation-independent, but do not accept
        // missing or nondeterministic collations as alternate index semantics.
        if (*index).rd_indcollation.is_null()
            || *(*index).rd_indcollation == pg_sys::InvalidOid
            || !pg_sys::get_collation_isdeterministic(*(*index).rd_indcollation)
        {
            pgrx::error!("postings_v1 requires a valid deterministic collation");
        }
        function
    }
}

unsafe fn validate_postings_key(
    index: pg_sys::Relation,
    key: &pg_sys::ScanKeyData,
    function: pg_sys::Oid,
) {
    // InvalidOid subtype means the opclass's input type (already checked text).
    // Validate ALL keys before NULL shortcuts or unsupported-query fallback;
    // neither may conceal a foreign operator or reinterpret an integer Datum.
    if key.sk_attno != 1
        || key.sk_strategy != 1
        || (key.sk_subtype != pg_sys::InvalidOid && key.sk_subtype != pg_sys::TEXTOID)
        || key.sk_func.fn_oid != function
        || key.sk_flags & !(pg_sys::SK_ISNULL as i32) != 0
    {
        pgrx::error!("postings_v1 requires canonical text-search scan keys");
    }
    if key.sk_collation != unsafe { *(*index).rd_indcollation } {
        pgrx::error!("postings_v1 scan collation does not match index collation");
    }
}

fn checked<T>(result: Result<T, String>) -> T {
    result.unwrap_or_else(|message| pgrx::error!("{message}"))
}

unsafe fn validate_heap(heap: pg_sys::Relation) {
    if unsafe { (*(*heap).rd_rel).relpersistence } != pg_sys::RELPERSISTENCE_PERMANENT as i8 {
        pgrx::error!(
            "postings_v1 requires a permanent heap; temporary/unlogged relations are unsupported"
        );
    }
}

/// Nonempty persisted identity wins over ALTER INDEX reloptions. Never create a
/// metapage from scan/insert: a zero-page legacy index may cover an existing heap.
unsafe fn persisted_mode(index: pg_sys::Relation) -> bool {
    if unsafe { crate::storage::stats(index) }.is_some() {
        unsafe { crate::options::validate_postings(index) };
        true
    } else if unsafe { crate::options::postings_requested(index) } {
        pgrx::error!(
            "postings_v1 metadata is missing; ALTER INDEX cannot convert a heap-baseline index; REINDEX is required"
        );
    } else {
        false
    }
}

#[derive(Default)]
struct BuildState {
    tuples: u64,
    postings: bool,
    builder: crate::term_index::Builder,
}

#[pg_guard]
unsafe extern "C-unwind" fn ambuild(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let postings = unsafe { crate::options::postings_requested(index) };
    if postings {
        unsafe {
            validate_heap(heap);
            crate::options::validate_postings(index);
            validate_postings_semantics(index);
        }
        if unsafe { (*index_info).ii_Concurrent } {
            pgrx::error!(
                "postings_v1 does not support CREATE INDEX CONCURRENTLY or REINDEX CONCURRENTLY"
            );
        }
    }
    // Registered with PostgreSQL, so callback ERROR / cancellation cannot leak
    // the Rust term map even if PostgreSQL abandons the C build stack.
    let state = pgrx::PgMemoryContexts::CurrentMemoryContext.leak_and_drop_on_delete(BuildState {
        postings,
        ..BuildState::default()
    });
    let heap_tuples = unsafe {
        pg_sys::table_index_build_scan(
            heap,
            index,
            index_info,
            true,
            true,
            Some(build_callback),
            state.cast(),
            std::ptr::null_mut(),
        )
    };
    if postings {
        let builder = unsafe { std::mem::take(&mut (*state).builder) };
        let payload = checked(builder.finish());
        unsafe {
            crate::storage::build(index, &payload);
        }
    }
    let mut result = unsafe { PgBox::<pg_sys::IndexBuildResult>::alloc0() };
    result.heap_tuples = heap_tuples;
    result.index_tuples = unsafe { (*state).tuples as f64 };
    result.into_pg_boxed().into_pg()
}

unsafe fn add_document(
    builder: &mut crate::term_index::Builder,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
) {
    if unsafe { *isnull } {
        return;
    }
    let document = unsafe { <&str as pgrx::FromDatum>::from_datum(*values, false) }
        .expect("nonnull index text datum");
    // The build callback supplies the HOT root TID; never substitute t_self.
    let tid = unsafe { &*tid };
    let block = (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo);
    let ctid = plumb_postings::Ctid::new(block, tid.ip_posid)
        .unwrap_or_else(|e| pgrx::error!("invalid postings_v1 CTID: {e}"));
    checked(builder.add(document, ctid));
}

#[pg_guard]
unsafe extern "C-unwind" fn build_callback(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    let state = unsafe { &mut *state.cast::<BuildState>() };
    state.tuples += 1;
    pgrx::check_for_interrupts!();
    if state.postings {
        unsafe {
            add_document(&mut state.builder, tid, values, isnull);
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn ambuildempty(index: pg_sys::Relation) {
    // This is PostgreSQL's unlogged INIT-fork callback, not the normal TRUNCATE
    // rebuild (which calls ambuild). No silent empty postings index for unlogged.
    if unsafe {
        crate::options::postings_requested(index) || crate::storage::stats(index).is_some()
    } {
        pgrx::error!("postings_v1 does not support unlogged INIT forks");
    }
}

#[pg_guard]
#[expect(
    clippy::too_many_arguments,
    reason = "PostgreSQL index AM callback signature"
)]
unsafe extern "C-unwind" fn aminsert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    index_info: *mut pg_sys::IndexInfo,
) -> bool {
    if unsafe { persisted_mode(index) } {
        unsafe {
            validate_heap(heap);
            validate_postings_semantics(index);
        }
        if !index_info.is_null() && unsafe { (*index_info).ii_Concurrent } {
            pgrx::error!("postings_v1 concurrent index maintenance is unsupported");
        }
        let mut builder = crate::term_index::Builder::default();
        unsafe {
            add_document(&mut builder, heap_tid, values, isnull);
        }
        let payload = checked(builder.finish());
        // NULL and tokenless documents cannot match supported positive queries.
        // Unsupported query shapes always visit the heap, including these rows.
        if !payload.is_empty() {
            unsafe {
                crate::storage::append(index, &payload);
            }
        }
    }
    false
}

#[pg_guard]
unsafe extern "C-unwind" fn ambeginscan(
    index: pg_sys::Relation,
    nkeys: i32,
    norderbys: i32,
) -> pg_sys::IndexScanDesc {
    // No Rust-owned opaque scan allocation: all candidate state is invocation-
    // local, keys remain in PostgreSQL's scan context, errors cannot leak a Box.
    unsafe { pg_sys::RelationGetIndexScan(index, nkeys, norderbys) }
}

#[pg_guard]
unsafe extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: i32,
    _orderbys: pg_sys::ScanKey,
    _norderbys: i32,
) {
    if !keys.is_null() {
        if nkeys != unsafe { (*scan).numberOfKeys } || nkeys < 0 {
            pgrx::error!("invalid plumb rescan key count");
        }
        // ScanKey arguments are executor-owned Datums, valid until next rescan.
        // PG AM API requires copying the structs, not retaining the input array.
        // ptr::copy also permits keys == scan.keyData and other overlap.
        unsafe {
            std::ptr::copy(keys, (*scan).keyData, nkeys as usize);
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn amgetbitmap(
    scan: pg_sys::IndexScanDesc,
    bitmap: *mut pg_sys::TIDBitmap,
) -> i64 {
    let index = unsafe { (*scan).indexRelation };
    let postings = unsafe { persisted_mode(index) };
    let function = if postings {
        unsafe { validate_postings_semantics(index) }
    } else {
        pg_sys::InvalidOid
    };
    let heap_oid = unsafe { (*(*index).rd_index).indrelid };
    let heap = unsafe { pgrx::PgRelation::with_lock(heap_oid, pg_sys::AccessShareLock as _) };
    if postings {
        unsafe {
            validate_heap(heap.as_ptr());
        }
    }
    let nkeys = unsafe { (*scan).numberOfKeys };
    if nkeys <= 0 {
        return 0;
    }
    if postings && nkeys > 256 {
        pgrx::error!("postings_v1 scan exceeds 256 key limit");
    }
    let keys = unsafe { std::slice::from_raw_parts((*scan).keyData, nkeys as usize) };
    if postings {
        for key in keys {
            pgrx::check_for_interrupts!();
            unsafe { validate_postings_key(index, key, function) };
        }
    }
    if keys
        .iter()
        .any(|key| key.sk_flags & pg_sys::SK_ISNULL as i32 != 0)
    {
        return 0;
    }
    if postings {
        let mut queries = Vec::new();
        let mut terms = std::collections::BTreeMap::new();
        let mut supported = true;
        for key in keys {
            pgrx::check_for_interrupts!();
            let text = unsafe { <&str as pgrx::FromDatum>::from_datum(key.sk_argument, false) }
                .expect("nonnull query key");
            if let Some(query) = checked(crate::term_index::query(text)) {
                crate::term_index::collect_terms(&query, &mut terms);
                if terms.len() > 256 {
                    pgrx::error!("postings_v1 scan exceeds 256 distinct query terms");
                }
                queries.push(query);
            } else {
                supported = false;
                break;
            }
        }
        if supported {
            unsafe {
                crate::storage::visit(index, |payload| {
                    checked(crate::term_index::accumulate(payload, &mut terms))
                });
            }
            let mut result: Option<plumb_postings::Postings> = None;
            for query in queries {
                pgrx::check_for_interrupts!();
                let next = checked(crate::term_index::evaluate(&query, &terms));
                result = Some(match result {
                    Some(old) => old.intersection(&next),
                    None => next,
                });
            }
            let result = result.unwrap_or_default();
            for (n, ctid) in result.iter().enumerate() {
                if n % 1024 == 0 {
                    pgrx::check_for_interrupts!();
                }
                // Match PostgreSQL's MaxHeapTuplesPerPage; the scalar codec is
                // intentionally more general and accepts every nonzero u16.
                let alignment = pg_sys::MAXIMUM_ALIGNOF as usize;
                let tuple_header = std::mem::offset_of!(pg_sys::HeapTupleHeaderData, t_bits);
                let aligned_header = (tuple_header + alignment - 1) & !(alignment - 1);
                let max_offset = (pg_sys::BLCKSZ as usize
                    - std::mem::offset_of!(pg_sys::PageHeaderData, pd_linp))
                    / (aligned_header + std::mem::size_of::<pg_sys::ItemIdData>());
                if usize::from(ctid.offset()) > max_offset {
                    pgrx::error!("postings_v1 frame contains an impossible heap tuple offset");
                }
                let mut tid = pg_sys::ItemPointerData {
                    ip_blkid: pg_sys::BlockIdData {
                        bi_hi: (ctid.block() >> 16) as u16,
                        bi_lo: ctid.block() as u16,
                    },
                    ip_posid: ctid.offset(),
                };
                unsafe {
                    pg_sys::tbm_add_tuples(bitmap, &mut tid, 1, true);
                }
            }
            return result.len() as i64;
        }
    }
    let heap_blocks = unsafe {
        pg_sys::RelationGetNumberOfBlocksInFork(heap.as_ptr(), pg_sys::ForkNumber::MAIN_FORKNUM)
    };
    // Unsupported whole query: all-heap-page recheck, never unsafe subtraction.
    for block in 0..heap_blocks {
        pgrx::check_for_interrupts!();
        unsafe {
            pg_sys::tbm_add_page(bitmap, block);
        }
    }
    i64::from(heap_blocks) * 10
}

#[pg_guard]
unsafe extern "C-unwind" fn amendscan(_scan: pg_sys::IndexScanDesc) {}

#[pg_extern]
fn index_stats(index: pgrx::PgRelation) -> pgrx::JsonB {
    let relation = index.as_ptr();
    unsafe {
        if (*(*relation).rd_rel).relkind != pg_sys::RELKIND_INDEX as i8
            || (*(*relation).rd_rel).relam != pg_sys::get_am_oid(c"plumb".as_ptr(), false)
        {
            pgrx::error!("plumb.index_stats requires an index using the plumb access method");
        }
        let (format, version, segments, payload, blocks) = match crate::storage::stats(relation) {
            Some(s) => (
                "postings_v1",
                s.format_version,
                s.segments,
                s.payload_bytes,
                s.relation_blocks,
            ),
            None => ("heap", 0, 0, 0, 0),
        };
        pgrx::JsonB(format!("{{\"storage\":\"{format}\",\"format_version\":{version},\"segments\":{segments},\"payload_bytes\":{payload},\"relation_blocks\":{blocks}}}")
            .parse().expect("valid statistics JSON"))
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn ambulkdelete(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _callback_state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    stats
}

#[pg_guard]
unsafe extern "C-unwind" fn amvacuumcleanup(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    stats
}

#[pg_guard]
#[expect(
    clippy::too_many_arguments,
    reason = "PostgreSQL index AM callback signature"
)]
unsafe extern "C-unwind" fn amcostestimate(
    _root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    let tuples = unsafe { (*(*(*path).indexinfo).rel).tuples.max(1.0) };
    unsafe {
        *startup = pg_sys::seq_page_cost * loop_count;
        *total = *startup + tuples * pg_sys::cpu_operator_cost * loop_count;
        *selectivity = 0.1;
        *correlation = 0.0;
        *pages = (tuples / 512.0).ceil();
    }
}
