//! Scans and the per-backend index cache.
//!
//! * **Bitmap scans** (`amgetbitmap`): `WHERE col ==> 'query'` and
//!   `WHERE col ~> 'search box'`.
//! * **Ordered scans** (`amgettuple`): `ORDER BY col <~> 'search box'`,
//!   tier by tier through [`tin_core::rank::Ranked`], so `LIMIT k` stops
//!   the scan as soon as it has k rows. Also plain index scans.
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
//! Liveness bitmaps and the pending list are reference-counted, so a scan
//! that spans many `amgettuple` calls keeps a consistent snapshot cheaply.
//!
//! Tids are handed to the executor as exact (`recheck = false`), except for
//! `*fragment*` patterns answered through grams, and for phrases and
//! proximity (the index only has their terms; the recheck has positions).
//! That is sound because VACUUM clears a dead tuple's liveness bit (or drops
//! its pending record) in `ambulkdelete`, before the heap can reuse its line
//! pointer. A scan whose cached liveness predates such a VACUUM still only
//! returns tids of tuples that were dead when the scan's snapshot was taken,
//! and any tuple later placed in a reused slot is invisible to that snapshot,
//! so the heap's visibility check drops it.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use pgrx::prelude::*;
use tin_core::rank::{Hit, Ranked, Sources, NO_TIER};
use tin_core::search::MAX_TIER;
use tin_core::{Analyzer, Plan, SearchBox, Segment};

use crate::pending::{self, Record};
use crate::storage::{self, Meta, MetaLock, PendingPos, SegmentRef};

/// Strategy of `~>` in the operator class (`lib.rs`); 1 is `==>`, 3 is
/// `<~>` (ORDER BY only).
const STRATEGY_SEARCH: u16 = 2;

/// A decoded index, as of some metapage state.
pub struct IndexState {
    generation: u64,
    pending_pos: PendingPos,
    loaded: HashMap<u32, Rc<Segment>>,
    /// In metapage order, with liveness bitmaps.
    pub segments: Vec<(SegmentRef, Rc<Segment>, Rc<Vec<u64>>)>,
    pub pending: Rc<Vec<Record>>,
}

type CacheKey = (pg_sys::Oid, pg_sys::RelFileNumber);

thread_local! {
    static CACHE: RefCell<HashMap<CacheKey, Rc<RefCell<IndexState>>>> = RefCell::new(HashMap::new());
}

/// Load `seg`'s segment (not its liveness): mapped from shared memory when
/// possible (see `shared`).
pub unsafe fn load_segment(index: pg_sys::Relation, seg: &SegmentRef) -> Segment {
    crate::shared::segment(index, seg)
}

/// Decode `seg`'s segment into this backend's own memory.
pub unsafe fn load_private(index: pg_sys::Relation, seg: &SegmentRef) -> Segment {
    let bytes = storage::read_blob(index, seg.first_block, seg.n_blocks, seg.len);
    Segment::from_bytes(&bytes)
        .unwrap_or_else(|e| error!("tin: segment {} at block {} is corrupt: {e}", seg.id, seg.first_block))
}

/// Drop every cached index (at backend exit, before shared memory goes).
pub fn clear_cache() {
    CACHE.with(|c| c.borrow_mut().clear());
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
                    pending: Rc::new(Vec::new()),
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
            // Copies the list only if a running scan still holds it.
            Rc::make_mut(&mut st.pending).extend(pending::decode_all(&bytes));
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
        segments.push((*r, seg, Rc::new(live)));
    }
    let (bytes, pos) = storage::read_pending(index, &meta, PendingPos::start(&meta));
    drop(lock);

    st.loaded = segments.iter().map(|(r, s, _)| (r.id, s.clone())).collect();
    st.segments = segments;
    st.pending = Rc::new(pending::decode_all(&bytes));
    st.pending_pos = pos;
    st.generation = meta.generation;
}

pub fn parse_query(q: &str) -> Plan {
    Plan::from_query(&parse_tinql(q)).unwrap_or_else(|e| error!("tin: invalid query {q:?}: {e}"))
}

pub fn parse_tinql(q: &str) -> tin_core::Query {
    tin_core::Query::parse(q, &mut Analyzer::new())
        .unwrap_or_else(|e| error!("tin: invalid query {q:?}: {e}"))
}

/// A search box, with this session's `tin.search_typos`.
pub fn parse_search(q: &str) -> SearchBox {
    SearchBox::parse(q, &mut Analyzer::new()).with_max_typos(SEARCH_TYPOS.get() as u8)
}

pub static SEARCH_TYPOS: pgrx::GucSetting<i32> = pgrx::GucSetting::<i32>::new(2);

/// One index condition.
#[derive(Clone, PartialEq)]
enum Cond {
    Match(Plan),
    Search(SearchBox),
}

impl Cond {
    fn plan(&self) -> Plan {
        match self {
            Cond::Match(p) => p.clone(),
            Cond::Search(s) => s.plan(MAX_TIER),
        }
    }
}

/// The scan's conditions; `None` if one compares with NULL (no rows).
unsafe fn conds(scan: pg_sys::IndexScanDesc) -> Option<Vec<Cond>> {
    let mut out = Vec::new();
    for i in 0..(*scan).numberOfKeys as usize {
        let key = &*(*scan).keyData.add(i);
        if key.sk_flags & pg_sys::SK_ISNULL as i32 != 0 {
            return None;
        }
        let q = String::from_datum(key.sk_argument, false).unwrap_or_default();
        out.push(match key.sk_strategy {
            STRATEGY_SEARCH => Cond::Search(parse_search(&q)),
            _ => Cond::Match(parse_query(&q)),
        });
    }
    Some(out)
}

fn and(plans: Vec<Plan>) -> Option<Plan> {
    match plans.len() {
        0 => None,
        1 => plans.into_iter().next(),
        _ => Some(Plan::And(plans)),
    }
}

/// A scan's snapshot of the index plus its progress.
struct ScanState {
    segments: Vec<(Rc<Segment>, Rc<Vec<u64>>)>,
    pending: Rc<Vec<Record>>,
    /// `None`: no rows (a NULL condition).
    ranked: Option<Ranked>,
    buf: VecDeque<Hit>,
}

/// Hits pulled from `Ranked` per refill.
const REFILL: usize = 16;

#[pg_guard]
pub unsafe extern "C-unwind" fn ambeginscan(
    index: pg_sys::Relation,
    nkeys: std::ffi::c_int,
    norderbys: std::ffi::c_int,
) -> pg_sys::IndexScanDesc {
    let scan = pg_sys::RelationGetIndexScan(index, nkeys, norderbys);
    if norderbys > 0 {
        let n = norderbys as usize;
        (*scan).xs_orderbyvals = pg_sys::palloc0(n * std::mem::size_of::<pg_sys::Datum>()).cast();
        (*scan).xs_orderbynulls = pg_sys::palloc0(n * std::mem::size_of::<bool>()).cast();
    }
    scan
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: std::ffi::c_int,
    orderbys: pg_sys::ScanKey,
    norderbys: std::ffi::c_int,
) {
    if !keys.is_null() && nkeys > 0 {
        std::ptr::copy(keys, (*scan).keyData, nkeys as usize);
    }
    if !orderbys.is_null() && norderbys > 0 {
        std::ptr::copy(orderbys, (*scan).orderByData, norderbys as usize);
    }
    free_state(scan);
}

unsafe fn free_state(scan: pg_sys::IndexScanDesc) {
    if !(*scan).opaque.is_null() {
        drop(Box::from_raw((*scan).opaque as *mut ScanState));
        (*scan).opaque = std::ptr::null_mut();
    }
}

/// Set up the ranked scan (on the first `amgettuple` after a rescan, when
/// the keys are final).
unsafe fn begin(scan: pg_sys::IndexScanDesc) -> ScanState {
    let st = state((*scan).indexRelation);
    let st = st.borrow();
    let segments = st.segments.iter().map(|(_, s, l)| (s.clone(), l.clone())).collect();
    let pending = st.pending.clone();
    drop(st);

    let mut order = None;
    let mut null_order = false;
    for i in 0..(*scan).numberOfOrderBys as usize {
        let key = &*(*scan).orderByData.add(i);
        if key.sk_flags & pg_sys::SK_ISNULL as i32 != 0 {
            null_order = true;
        } else if order.is_none() {
            order = Some(parse_search(&String::from_datum(key.sk_argument, false).unwrap_or_default()));
        }
    }
    let ranked = conds(scan).map(|conds| {
        let query = order.clone().unwrap_or_else(|| parse_search(""));
        // A search-box scan whose only conditions are the same search needs
        // no filter: every tier row satisfies them (and tiers can stream).
        let filter =
            if !null_order && order.is_some() && conds.iter().all(|c| *c == Cond::Search(query.clone())) {
                None
            } else {
                and(conds.iter().map(Cond::plan).collect())
            };
        Ranked::new(query, filter)
    });
    ScanState { segments, pending, ranked, buf: VecDeque::new() }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amgettuple(
    scan: pg_sys::IndexScanDesc,
    dir: pg_sys::ScanDirection::Type,
) -> bool {
    if dir != pg_sys::ScanDirection::ForwardScanDirection {
        error!("tin: index scans only go forward");
    }
    if (*scan).opaque.is_null() {
        (*scan).opaque = Box::into_raw(Box::new(begin(scan))).cast();
    }
    let s = &mut *((*scan).opaque as *mut ScanState);
    let Some(ranked) = s.ranked.as_mut() else { return false };
    if s.buf.is_empty() {
        let src = Sources {
            segments: s.segments.iter().map(|(seg, live)| (&**seg, Some(live.as_slice()))).collect(),
            pending: &s.pending,
        };
        while s.buf.len() < REFILL {
            match ranked.next(&src) {
                Some(h) => s.buf.push_back(h),
                None => break,
            }
        }
    }
    let Some(hit) = s.buf.pop_front() else { return false };
    pgrx::itemptr::item_pointer_set_all(&mut (*scan).xs_heaptid, hit.tid.block, hit.tid.offset);
    (*scan).xs_recheck = hit.recheck;
    if (*scan).numberOfOrderBys > 0 {
        let d = if hit.tier == NO_TIER { f64::INFINITY } else { hit.tier as f64 };
        for i in 0..(*scan).numberOfOrderBys as usize {
            *(*scan).xs_orderbyvals.add(i) = d.into_datum().unwrap();
            *(*scan).xs_orderbynulls.add(i) = false;
        }
        (*scan).xs_recheckorderby = hit.tier_is_bound;
    }
    true
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amendscan(scan: pg_sys::IndexScanDesc) {
    free_state(scan);
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amgetbitmap(scan: pg_sys::IndexScanDesc, tbm: *mut pg_sys::TIDBitmap) -> i64 {
    let Some(conds) = conds(scan) else { return 0 }; // col ==> NULL matches nothing
    let Some(plan) = and(conds.iter().map(Cond::plan).collect()) else {
        error!("tin: a scan needs at least one ==> or ~> condition")
    };

    // Fragments resolved through grams yield candidates: let the executor
    // recheck them.
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
    for rec in st.pending.iter() {
        if pending::matches(rec, &plan) {
            emit(rec.tid);
        }
    }
    flush(&mut batch);
    n
}

/// Pending records [`selectivity`] evaluates; the rest are assumed alike.
const PENDING_SAMPLE: usize = 1000;

/// Planner estimate: the share of indexed tuples matching `plan`, plus
/// `extra_rows`.
pub unsafe fn selectivity(index: pg_sys::Relation, plan: &Plan, extra_rows: f64) -> f64 {
    let st = state(index);
    let st = st.borrow();
    let (mut docs, mut hits) = (0.0, 0.0);
    for (_, seg, _) in &st.segments {
        docs += seg.meta().doc_count as f64;
        hits += seg.estimate(plan);
    }
    let sample = &st.pending[..st.pending.len().min(PENDING_SAMPLE)];
    if !sample.is_empty() {
        let matched = sample.iter().filter(|r| pending::matches(r, plan)).count();
        hits += matched as f64 * st.pending.len() as f64 / sample.len() as f64;
        docs += st.pending.len() as f64;
    }
    if docs == 0.0 {
        return 0.0;
    }
    ((hits + extra_rows) / docs).clamp(0.0, 1.0)
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
