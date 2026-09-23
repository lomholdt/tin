//! Bitmap scans (`WHERE col ==> 'query'` → `amgetbitmap`) and the
//! per-backend index cache.
//!
//! Each backend keeps a decoded copy of every index it has scanned, keyed by
//! (OID, relfilenumber): REINDEX, TRUNCATE and VACUUM FULL assign a new
//! relfilenumber, so older copies are never consulted. Within one relfile the
//! metapage says what changed:
//!
//! * `generation` moved (flush or VACUUM): reload every segment's liveness
//!   bitmap and the whole pending list; load segments not seen before.
//!   Segments themselves never change, so cached ones are reused by id.
//! * only `pending_bytes` grew (inserts): read just the new records.
//!
//! Tids are handed to the executor as exact (`recheck = false`), except for
//! plans with `*fragment*` patterns answered through trigrams. That is
//! sound because VACUUM clears a dead tuple's liveness bit (or drops its
//! pending record) in `ambulkdelete`, before the heap can reuse its line
//! pointer. A scan whose cached liveness predates such a VACUUM still only
//! returns tids of tuples that were dead when the scan's snapshot was taken,
//! and any tuple later placed in a reused slot is invisible to that snapshot,
//! so the heap's visibility check drops it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pgrx::prelude::*;
use tin_core::{Analyzer, Plan, Segment};

use crate::pending::{self, Record};
use crate::storage::{self, Meta, MetaLock, PendingPos, SegmentRef};

/// A decoded index, as of some metapage state.
pub struct IndexState {
    generation: u64,
    pending_pos: PendingPos,
    loaded: HashMap<u32, Rc<Segment>>,
    /// In metapage order.
    pub segments: Vec<(SegmentRef, Rc<Segment>, Vec<u64>)>,
    pub pending: Vec<Record>,
}

type CacheKey = (pg_sys::Oid, pg_sys::RelFileNumber);

thread_local! {
    static CACHE: RefCell<HashMap<CacheKey, Rc<RefCell<IndexState>>>> = RefCell::new(HashMap::new());
}

/// Load `seg`'s segment (not its liveness).
pub unsafe fn load_segment(index: pg_sys::Relation, seg: &SegmentRef) -> Segment {
    let bytes = storage::read_blob(index, seg.first_block, seg.n_blocks, seg.len);
    Segment::from_bytes(&bytes)
        .unwrap_or_else(|e| error!("tin: segment {} at block {} is corrupt: {e}", seg.id, seg.first_block))
}

/// Liveness words of a segment with `tuple_bits` positions.
pub fn live_words(seg: &Segment) -> usize {
    (seg.tuple_bits() as usize).div_ceil(64)
}

/// This backend's up-to-date copy of `index`.
pub unsafe fn state(index: pg_sys::Relation) -> Rc<RefCell<IndexState>> {
    let key = ((*index).rd_id, (*index).rd_locator.relNumber);
    let st = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if !c.contains_key(&key) {
            // Drop older generations of the same index.
            c.retain(|k, _| k.0 != key.0);
            c.insert(
                key,
                Rc::new(RefCell::new(IndexState {
                    generation: u64::MAX,
                    pending_pos: PendingPos::default(),
                    loaded: HashMap::new(),
                    segments: Vec::new(),
                    pending: Vec::new(),
                })),
            );
        }
        c[&key].clone()
    });
    refresh(index, &mut st.borrow_mut());
    st
}

unsafe fn refresh(index: pg_sys::Relation, st: &mut IndexState) {
    let lock = MetaLock::share(index);
    let meta = lock.read();
    if meta.generation == st.generation {
        if meta.pending_bytes != st.pending_pos.total {
            let (bytes, pos) = storage::read_pending(index, &meta, st.pending_pos);
            st.pending.extend(pending::decode_all(&bytes));
            st.pending_pos = pos;
        }
        return;
    }
    // Segments are immutable: reuse the ones we have, load the rest. (Loaded
    // under the shared metapage lock for simplicity; that briefly holds up
    // inserters the first time a backend reads a large index.)
    let mut segments = Vec::with_capacity(meta.segments.len());
    for r in &meta.segments {
        let seg = match st.loaded.get(&r.id) {
            Some(s) => s.clone(),
            None => Rc::new(load_segment(index, r)),
        };
        let live = storage::read_liveness(index, r, live_words(&seg));
        segments.push((*r, seg, live));
    }
    let (bytes, pos) = storage::read_pending(index, &meta, PendingPos::start(&meta));
    drop(lock);

    st.loaded = segments.iter().map(|(r, s, _)| (r.id, s.clone())).collect();
    st.segments = segments;
    st.pending = pending::decode_all(&bytes);
    st.pending_pos = pos;
    st.generation = meta.generation;
}

pub fn parse_query(q: &str) -> Plan {
    Plan::parse(q, &mut Analyzer::new()).unwrap_or_else(|e| error!("tin: invalid query {q:?}: {e}"))
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambeginscan(
    index: pg_sys::Relation,
    nkeys: std::ffi::c_int,
    norderbys: std::ffi::c_int,
) -> pg_sys::IndexScanDesc {
    pg_sys::RelationGetIndexScan(index, nkeys, norderbys)
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: std::ffi::c_int,
    _orderbys: pg_sys::ScanKey,
    _norderbys: std::ffi::c_int,
) {
    if !keys.is_null() && nkeys > 0 {
        std::ptr::copy(keys, (*scan).keyData, nkeys as usize);
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amendscan(_scan: pg_sys::IndexScanDesc) {}

#[pg_guard]
pub unsafe extern "C-unwind" fn amgetbitmap(scan: pg_sys::IndexScanDesc, tbm: *mut pg_sys::TIDBitmap) -> i64 {
    let mut plans = Vec::new();
    for i in 0..(*scan).numberOfKeys as usize {
        let key = &*(*scan).keyData.add(i);
        if key.sk_flags & pg_sys::SK_ISNULL as i32 != 0 {
            return 0; // col ==> NULL matches nothing
        }
        let q = String::from_datum(key.sk_argument, false).unwrap_or_default();
        plans.push(parse_query(&q));
    }
    let plan = match plans.len() {
        0 => error!("tin: a scan needs at least one ==> condition"),
        1 => plans.pop().unwrap(),
        _ => Plan::And(plans),
    };

    // Fragments resolved through grams yield candidates: let the executor
    // recheck them with `tin_match`.
    let recheck = plan.needs_recheck();
    let st = state((*scan).indexRelation);
    let st = st.borrow();
    const BATCH: usize = 4096;
    let mut batch: Vec<pg_sys::ItemPointerData> = Vec::with_capacity(BATCH);
    let mut n = 0i64;
    let flush = |batch: &mut Vec<pg_sys::ItemPointerData>| {
        if !batch.is_empty() {
            pg_sys::tbm_add_tuples(tbm, batch.as_mut_ptr(), batch.len() as i32, recheck);
            batch.clear();
        }
    };
    let mut emit = |t: tin_core::Tid| {
        let mut ip = pg_sys::ItemPointerData::default();
        pgrx::itemptr::item_pointer_set_all(&mut ip, t.block, t.offset);
        batch.push(ip);
        n += 1;
        if batch.len() == BATCH {
            flush(&mut batch);
        }
    };
    for (_, seg, live) in &st.segments {
        seg.search_live(&plan, Some(live), &mut emit);
    }
    for rec in &st.pending {
        if rec.matches(&plan) {
            emit(rec.tid);
        }
    }
    flush(&mut batch);
    n
}

/// Pending records [`selectivity`] evaluates; the rest are assumed alike.
const PENDING_SAMPLE: usize = 1000;

/// Planner estimate: the share of indexed tuples matching `plan`.
pub unsafe fn selectivity(index: pg_sys::Relation, plan: &Plan) -> f64 {
    let st = state(index);
    let st = st.borrow();
    let (mut docs, mut hits) = (0.0, 0.0);
    for (_, seg, _) in &st.segments {
        docs += seg.meta().doc_count as f64;
        hits += seg.estimate(plan);
    }
    let sample = &st.pending[..st.pending.len().min(PENDING_SAMPLE)];
    if !sample.is_empty() {
        let matched = sample.iter().filter(|r| r.matches(plan)).count();
        hits += matched as f64 * st.pending.len() as f64 / sample.len() as f64;
        docs += st.pending.len() as f64;
    }
    if docs == 0.0 {
        return 0.0;
    }
    (hits / docs).clamp(0.0, 1.0)
}

/// Totals for `tin_stats`.
pub unsafe fn stats(index: pg_sys::Relation) -> (Meta, Vec<u64>) {
    let st = state(index);
    let st = st.borrow();
    let live: Vec<u64> =
        st.segments.iter().map(|(_, _, l)| l.iter().map(|w| w.count_ones() as u64).sum()).collect();
    let meta = MetaLock::share(index).read();
    (meta, live)
}
