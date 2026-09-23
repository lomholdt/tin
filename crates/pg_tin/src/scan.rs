//! Bitmap scans: `WHERE col ==> 'query'` → `amgetbitmap`.
//!
//! The index is immutable in Phase 1, so each backend loads it once and keeps
//! it, keyed by relation OID *and* relfilenumber: REINDEX, TRUNCATE and
//! VACUUM FULL all assign a new relfilenumber, so a stale copy is never used.
//!
//! Tids are handed to the executor as exact (`recheck = false`). That is
//! sound while the index is read-only: a stale entry can only point at a
//! dead tuple (the heap's visibility check drops it) or at a reused line
//! pointer, and in Phase 1 every insert into the table fails in `aminsert`
//! and aborts, so a reused slot only ever holds an aborted tuple or a
//! heap-only tuple, which a bitmap heap scan never starts a HOT chain from.
//! Phase 2 adds liveness bitmaps before inserts are allowed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pgrx::prelude::*;
use tin_core::{Analyzer, Index, Plan};

use crate::storage;

thread_local! {
    static CACHE: RefCell<HashMap<(pg_sys::Oid, pg_sys::RelFileNumber), Rc<Index>>> =
        RefCell::new(HashMap::new());
}

/// This backend's copy of `index`, loading it on first use.
pub unsafe fn cached_index(index: pg_sys::Relation) -> Rc<Index> {
    let key = ((*index).rd_id, (*index).rd_locator.relNumber);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let loaded = Rc::new(storage::read_index(index));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        // Drop older generations of the same index.
        c.retain(|k, _| k.0 != key.0);
        c.insert(key, loaded.clone());
    });
    loaded
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

    let index = cached_index((*scan).indexRelation);
    const BATCH: usize = 4096;
    let mut batch: Vec<pg_sys::ItemPointerData> = Vec::with_capacity(BATCH);
    let mut n = 0i64;
    let flush = |batch: &mut Vec<pg_sys::ItemPointerData>| {
        if !batch.is_empty() {
            pg_sys::tbm_add_tuples(tbm, batch.as_mut_ptr(), batch.len() as i32, false);
            batch.clear();
        }
    };
    index.search(&plan, |t| {
        let mut ip = pg_sys::ItemPointerData::default();
        pgrx::itemptr::item_pointer_set_all(&mut ip, t.block, t.offset);
        batch.push(ip);
        n += 1;
        if batch.len() == BATCH {
            flush(&mut batch);
        }
    });
    flush(&mut batch);
    n
}
