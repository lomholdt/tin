//! Writes: inserts into the pending list, flushes into segments, and VACUUM.

use std::collections::HashSet;

use pgrx::prelude::*;
use pgrx::{GucSetting, PgBox};
use tin_core::{Analyzer, SegmentBuilder, Tid};

use crate::pending;
use crate::scan;
use crate::storage::{self, Meta, MetaLock, PendingPos, SegmentRef, MAX_SEGMENTS};

/// `tin.pending_list_limit` (kB): flush the pending list into a new segment
/// once it grows past this.
pub static PENDING_LIST_LIMIT: GucSetting<i32> = GucSetting::<i32>::new(4096);

#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn aminsert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    if *isnull {
        return false; // NULL never matches
    }
    let text = String::from_datum(*values, false).unwrap_or_default();
    let terms = Analyzer::new().unique_terms(&text);
    let (block, offset) = pgrx::itemptr::item_pointer_get_both(*heap_tid);
    let record = pending::encode(Tid::new(block, offset), &terms);

    let lock = MetaLock::exclusive(index);
    let mut meta = lock.read();
    storage::append_pending(index, &lock, &mut meta, &record);
    if meta.pending_bytes > PENDING_LIST_LIMIT.get() as u64 * 1024 {
        flush_locked(index, &lock, &mut meta);
    }
    false
}

/// Turn the pending list into a new segment. Caller holds the metapage
/// exclusively. Returns the number of records flushed.
pub unsafe fn flush_locked(index: pg_sys::Relation, lock: &MetaLock, meta: &mut Meta) -> u64 {
    if meta.pending_count == 0 {
        return 0;
    }
    if meta.segments.len() >= MAX_SEGMENTS {
        // Merges come in a later phase; until then the pending list keeps
        // growing (still correct, just slower) and REINDEX compacts.
        warning!(
            "tin: index \"{}\" has {MAX_SEGMENTS} segments; REINDEX it to compact",
            crate::build::name_of(index)
        );
        return 0;
    }
    let (bytes, _) = storage::read_pending(index, meta, PendingPos::start(meta));
    let mut records = pending::decode_all(&bytes);
    records.sort_by_key(|r| r.tid);
    records.dedup_by_key(|r| r.tid); // a tid is only reused after VACUUM removed its record

    let mut builder = SegmentBuilder::open_ended(records[0].tid.block);
    for r in &records {
        builder.add_terms(r.tid, r.terms.iter().map(String::as_str));
    }
    let seg = builder.finish();
    let seg_bytes = seg.to_bytes();
    let (first_block, n_blocks) = storage::write_blob(index, &seg_bytes, true);
    let (live_first, live_blocks) = storage::write_blob(index, &storage::words_to_bytes(seg.docs()), true);

    let old_pages = storage::pending_blocks(index, meta);
    meta.segments.push(SegmentRef {
        id: meta.next_segment_id,
        first_block,
        n_blocks,
        len: seg_bytes.len() as u64,
        live_first,
        live_blocks,
    });
    meta.next_segment_id += 1;
    storage::rewrite_pending(index, meta, &[], 0);
    meta.generation += 1;
    lock.write(index, meta);
    storage::free_pending_pages(index, &old_pages);
    records.len() as u64
}

/// `SELECT tin_flush('idx'::regclass)`: flush the pending list now.
pub unsafe fn flush(index: pg_sys::Relation) -> u64 {
    let lock = MetaLock::exclusive(index);
    let mut meta = lock.read();
    flush_locked(index, &lock, &mut meta)
}

struct Removed {
    removed: f64,
    remaining: f64,
}

/// Clear the liveness bits of `seg`'s dead tuples.
unsafe fn vacuum_segment(
    index: pg_sys::Relation,
    r: &SegmentRef,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut std::ffi::c_void,
    acc: &mut Removed,
) {
    let callback = callback.expect("ambulkdelete without a callback");
    let seg = scan::load_segment(index, r);
    let old = storage::read_liveness(index, r, scan::live_words(&seg));
    let mut new = old.clone();
    seg.for_each_set_tid(&old, |bit, tid| {
        let mut ip = pg_sys::ItemPointerData::default();
        pgrx::itemptr::item_pointer_set_all(&mut ip, tid.block, tid.offset);
        if callback(&mut ip, callback_state) {
            new[(bit >> 6) as usize] &= !(1u64 << (bit & 63));
            acc.removed += 1.0;
        } else {
            acc.remaining += 1.0;
        }
    });
    storage::update_liveness(index, r, &old, &new);
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambulkdelete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut std::ffi::c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let index = (*info).index;
    let stats =
        if stats.is_null() { PgBox::<pg_sys::IndexBulkDeleteResult>::alloc0().into_pg() } else { stats };
    let mut acc = Removed { removed: 0.0, remaining: 0.0 };

    // Pass 1, without blocking writers: the segments that exist now.
    let first = MetaLock::share(index).read();
    let mut done = HashSet::new();
    for r in &first.segments {
        vacuum_segment(index, r, callback, callback_state, &mut acc);
        done.insert(r.id);
    }

    // Pass 2, exclusively: segments flushed meanwhile, and the pending list.
    let lock = MetaLock::exclusive(index);
    let mut meta = lock.read();
    for r in meta.segments.clone().iter().filter(|r| !done.contains(&r.id)) {
        vacuum_segment(index, r, callback, callback_state, &mut acc);
    }
    let (bytes, _) = storage::read_pending(index, &meta, PendingPos::start(&meta));
    let records = pending::decode_all(&bytes);
    let cb = callback.unwrap();
    let mut kept = Vec::with_capacity(bytes.len());
    let mut kept_n = 0u64;
    for r in &records {
        let mut ip = pg_sys::ItemPointerData::default();
        pgrx::itemptr::item_pointer_set_all(&mut ip, r.tid.block, r.tid.offset);
        if cb(&mut ip, callback_state) {
            acc.removed += 1.0;
        } else {
            kept.extend_from_slice(&pending::encode(r.tid, &r.terms));
            kept_n += 1;
            acc.remaining += 1.0;
        }
    }
    let mut old_pages = Vec::new();
    if kept_n != records.len() as u64 {
        old_pages = storage::pending_blocks(index, &meta);
        storage::rewrite_pending(index, &mut meta, &kept, kept_n);
    }
    // Always bump: liveness pages changed, and backends must reload them.
    meta.generation += 1;
    lock.write(index, &meta);
    storage::free_pending_pages(index, &old_pages);
    drop(lock);

    (*stats).tuples_removed += acc.removed;
    (*stats).num_index_tuples = acc.remaining;
    (*stats).num_pages = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    stats
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amvacuumcleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let index = (*info).index;
    if (*info).analyze_only {
        return stats;
    }
    flush(index);
    pg_sys::IndexFreeSpaceMapVacuum(index);
    let stats = if stats.is_null() {
        let s = PgBox::<pg_sys::IndexBulkDeleteResult>::alloc0().into_pg();
        (*s).num_index_tuples = (*info).num_heap_tuples;
        (*s).estimated_count = (*info).estimated_count;
        s
    } else {
        stats
    };
    (*stats).num_pages = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    stats
}
