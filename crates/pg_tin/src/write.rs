//! Writes: inserts into the pending list, flushes into segments, and VACUUM.

use std::collections::HashSet;

use pgrx::prelude::*;
use pgrx::{GucSetting, PgBox};
use tin_core::{Analyzer, Segment, SegmentBuilder, Tid};

use crate::pending;
use crate::scan;
use crate::storage::{self, Meta, MetaLock, PendingPos, SegmentRef};

/// `tin.pending_list_limit` (kB): flush the pending list into a new segment
/// once it grows past this. 1 MB, not GIN's 4 MB: every search scans the
/// pending list, and under a write storm (5M rows, 4k updates/s) 1 MB gave
/// both the fastest writes and the lowest search p99 (6.7 ms vs 9.8 ms).
pub static PENDING_LIST_LIMIT: GucSetting<i32> = GucSetting::<i32>::new(1024);

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
    let full = meta.pending_bytes > PENDING_LIST_LIMIT.get() as u64 * 1024;
    drop(lock);
    if full {
        // Only one backend flushes at a time; the others just keep appending.
        flush(index, false);
    }
    false
}

/// The flush mutex: a heavyweight lock on block 0, like GIN's pending-list
/// cleanup. Held by flushes (build and install) and by VACUUM while it
/// rewrites the pending list or frees retired pages, so none of them sees
/// the others half done. Readers and inserters never take it.
const FLUSH_LOCK: u32 = 7; // ExclusiveLock
const FLUSH_BLOCK: u32 = 0;

/// The compaction lock (block 1): held exclusively for a whole compaction,
/// in share mode by a flush while it merges. Flushes don't wait for it; they
/// skip merging instead, so a long compaction never holds up flushes.
const COMPACT_BLOCK: u32 = 1;
const COMPACT_SHARE: u32 = 5; // ShareLock

struct FlushMutex(pg_sys::Relation);

impl FlushMutex {
    unsafe fn acquire(index: pg_sys::Relation, wait: bool) -> Option<FlushMutex> {
        if wait {
            pg_sys::LockPage(index, FLUSH_BLOCK, FLUSH_LOCK as pg_sys::LOCKMODE);
        } else if !pg_sys::ConditionalLockPage(index, FLUSH_BLOCK, FLUSH_LOCK as pg_sys::LOCKMODE) {
            return None;
        }
        Some(FlushMutex(index))
    }
}

impl Drop for FlushMutex {
    fn drop(&mut self) {
        // On error the transaction abort releases it instead.
        if !std::thread::panicking() {
            unsafe { pg_sys::UnlockPage(self.0, FLUSH_BLOCK, FLUSH_LOCK as pg_sys::LOCKMODE) };
        }
    }
}

/// Turn the pending list into a new segment (and merge full tiers). Returns
/// the number of records flushed; 0 if another backend is flushing and
/// `wait` is false.
///
/// The slow part (building and merging segments, writing their pages) runs
/// without the metapage lock, on a snapshot; the lock is taken only to
/// install the result, so searches and inserts wait milliseconds, not the
/// seconds a 64 MB merge takes. Records appended meanwhile stay pending.
pub unsafe fn flush(index: pg_sys::Relation, wait: bool) -> u64 {
    let Some(_mutex) = FlushMutex::acquire(index, wait) else { return 0 };

    // Snapshot: the segment table and the pending list up to `pos`.
    let (snap, bytes, pos) = {
        let lock = MetaLock::share(index);
        let meta = lock.read();
        if meta.pending_count == 0 {
            return 0;
        }
        let (bytes, pos) = storage::read_pending(index, &meta, PendingPos::start(&meta));
        (meta, bytes, pos)
    };
    let mut records = pending::decode_all(&bytes);
    records.sort_by_key(|r| r.tid);
    records.dedup_by_key(|r| r.tid); // a tid is only reused after VACUUM removed its record

    // Build, merge and write, unlocked, on a copy of the segment table.
    let mut next = snap.clone();
    let mut written = Written::default();
    if !next.can_add_segment() {
        // Only reachable with many large (unmergeable) segments plus pages
        // waiting for VACUUM: the pending list keeps growing (still correct,
        // just slower) until VACUUM or REINDEX makes room.
        warning!(
            "tin: index \"{}\" has no room for another segment; VACUUM or REINDEX it",
            crate::build::name_of(index)
        );
        return 0;
    }
    let mut builder =
        SegmentBuilder::open_ended(records[0].tid.block).with_grams(crate::options::grams(index));
    for r in &records {
        builder.add_terms(r.tid, r.terms.iter().map(String::as_str));
    }
    add_segment(index, &mut next, &builder.finish(), &mut written);
    // No merging while a compaction runs: it's rewriting those segments.
    if pg_sys::ConditionalLockPage(index, COMPACT_BLOCK, COMPACT_SHARE as pg_sys::LOCKMODE) {
        merge_small_segments(index, &mut next, &mut written);
        pg_sys::UnlockPage(index, COMPACT_BLOCK, COMPACT_SHARE as pg_sys::LOCKMODE);
    }

    if !install(index, &snap, next, Some(pos), &written) {
        return 0;
    }
    records.len() as u64
}

/// Install `next`'s segment table (built from `snap` without the metapage
/// lock), keeping records appended since `pending_pos`, or the whole pending
/// list if `None`. False (and `written` freed) if the index changed since
/// the snapshot: can't happen under the flush mutex, since only flushes and
/// VACUUM's pass 2, both under it, change the generation; if it did,
/// installing could resurrect tuples VACUUM just removed.
unsafe fn install(
    index: pg_sys::Relation,
    snap: &Meta,
    next: Meta,
    pending_pos: Option<PendingPos>,
    written: &Written,
) -> bool {
    let lock = MetaLock::exclusive(index);
    let mut meta = lock.read();
    if meta.generation != snap.generation {
        drop(lock);
        written.free(index);
        warning!("tin: index \"{}\" changed during a flush; retrying later", crate::build::name_of(index));
        return false;
    }
    let mut old_pages = Vec::new();
    if let Some(pos) = pending_pos {
        let (rest, _) = storage::read_pending(index, &meta, pos);
        let rest_count = meta.pending_count - snap.pending_count;
        old_pages = storage::pending_blocks(index, &meta);
        storage::rewrite_pending(index, &mut meta, &rest, rest_count);
    }
    meta.segments = next.segments;
    meta.retired = next.retired;
    meta.next_segment_id = next.next_segment_id;
    meta.generation += 1;
    debug_assert!(meta.fits());
    lock.write(index, &meta);
    drop(lock);
    written.publish(index, &meta);
    storage::free_pages(index, &old_pages);
    pg_sys::IndexFreeSpaceMapVacuum(index);
    true
}

/// Compaction, from VACUUM's cleanup (an autovacuum worker, not a query):
/// merge every segment into one when the others add up to more than
/// [`COMPACT_RATIO`] of the largest (e.g. the one `CREATE INDEX` built, which
/// is too big to merge inline) or there are more than [`COMPACT_SEGMENTS`].
/// That also drops the dead tuples the big segment accumulates. Needs the
/// whole index in memory twice over, so it only runs while the index fits
/// in half of `maintenance_work_mem`. Runs off the metapage lock, and holds
/// only the compaction lock while merging, so flushes carry on meanwhile.
const COMPACT_RATIO: f64 = 0.25;
const COMPACT_SEGMENTS: usize = 12;

unsafe fn compact(index: pg_sys::Relation) {
    pg_sys::LockPage(index, COMPACT_BLOCK, FLUSH_LOCK as pg_sys::LOCKMODE);
    compact_locked(index);
    pg_sys::UnlockPage(index, COMPACT_BLOCK, FLUSH_LOCK as pg_sys::LOCKMODE);
}

unsafe fn compact_locked(index: pg_sys::Relation) {
    let snap = MetaLock::share(index).read();
    let total: u64 = snap.segments.iter().map(|r| r.len).sum();
    let largest = snap.segments.iter().map(|r| r.len).max().unwrap_or(0);
    let budget = pg_sys::maintenance_work_mem as u64 * 1024;
    let worth =
        (total - largest) as f64 > COMPACT_RATIO * largest as f64 || snap.segments.len() > COMPACT_SEGMENTS;
    if snap.segments.len() < 2 || !worth || total * 2 > budget {
        return;
    }
    // Liveness is current: we run in VACUUM's cleanup, after its bulk delete,
    // and no other VACUUM of this index can start before we finish.
    let started = std::time::Instant::now();
    let segs: Vec<Segment> = snap.segments.iter().map(|r| scan::load_segment(index, r)).collect();
    let lives: Vec<Vec<u64>> = snap
        .segments
        .iter()
        .zip(&segs)
        .map(|(r, s)| storage::read_liveness(index, r, scan::live_words(s)))
        .collect();
    let inputs: Vec<(&Segment, Option<&[u64]>)> =
        segs.iter().zip(&lives).map(|(s, l)| (s, Some(l.as_slice()))).collect();
    let merged = Segment::merge(&inputs);
    drop(segs);
    let mut written = Written::default();
    let mut out = snap.clone();
    out.segments.clear();
    if let Some(seg) = merged {
        add_segment(index, &mut out, &seg, &mut written);
    }

    // Install: flushes kept adding segments meanwhile (without merging), so
    // keep those, and retire exactly the ones merged.
    let _mutex = FlushMutex::acquire(index, true);
    let lock = MetaLock::exclusive(index);
    let mut meta = lock.read();
    let merged_ids: HashSet<u32> = snap.segments.iter().map(|r| r.id).collect();
    let present = meta.segments.iter().filter(|r| merged_ids.contains(&r.id)).count();
    let mut next = meta.clone();
    next.segments.retain(|r| !merged_ids.contains(&r.id));
    for r in &snap.segments {
        next.retired.push((r.first_block, r.n_blocks));
        next.retired.push((r.live_first, r.live_blocks));
    }
    // New segment ids must not collide with ones flushed meanwhile.
    for mut r in out.segments {
        r.id = next.next_segment_id;
        next.next_segment_id += 1;
        next.segments.push(r);
    }
    if present != merged_ids.len() || !next.fits() {
        // Not expected (only compaction and flush merges, which it excludes,
        // remove segments); or too many retired chains to record until the
        // next VACUUM frees them. Give the pages back instead.
        drop(lock);
        written.free(index);
        return;
    }
    meta.segments = next.segments;
    meta.retired = next.retired;
    meta.next_segment_id = next.next_segment_id;
    meta.generation += 1;
    lock.write(index, &meta);
    drop(lock);
    written.publish(index, &meta);
    log!(
        "tin: compacted {} segments ({} MB) of index \"{}\" into one in {:.1} s",
        snap.segments.len(),
        total >> 20,
        crate::build::name_of(index),
        started.elapsed().as_secs_f64()
    );
}

/// Write `seg` (and its liveness) and add it to `meta`'s segment table;
/// record the chains in `written`.
unsafe fn add_segment(index: pg_sys::Relation, meta: &mut Meta, seg: &Segment, written: &mut Written) {
    let bytes = seg.to_bytes();
    let (first_block, n_blocks) = storage::write_blob(index, &bytes, true);
    let (live_first, live_blocks) = storage::write_blob(index, &storage::words_to_bytes(seg.docs()), true);
    written.chains.push((first_block, n_blocks));
    written.chains.push((live_first, live_blocks));
    meta.segments.push(SegmentRef {
        id: meta.next_segment_id,
        first_block,
        n_blocks,
        len: bytes.len() as u64,
        live_first,
        live_blocks,
    });
    meta.next_segment_id += 1;
    written.blobs.push((first_block, bytes));
}

/// What a flush, merge or compaction wrote: page chains (freed if it can't
/// install) and segment blobs (published to shared memory once installed,
/// so no reader has to copy them in).
#[derive(Default)]
struct Written {
    chains: Vec<(u32, u32)>,
    blobs: Vec<(u32, Vec<u8>)>,
}

impl Written {
    unsafe fn free(&self, index: pg_sys::Relation) {
        let pages: Vec<u32> =
            self.chains.iter().flat_map(|&(h, n)| storage::chain_blocks(index, h, n)).collect();
        storage::free_pages(index, &pages);
    }

    /// Publish the blobs of segments that made it into `meta`.
    unsafe fn publish(&self, index: pg_sys::Relation, meta: &Meta) {
        for (head, bytes) in &self.blobs {
            if meta.segments.iter().any(|r| r.first_block == *head) {
                crate::shared::publish(index, *head, bytes);
            }
        }
    }
}

/// Size tiers for merging: a segment of `len` bytes is in tier
/// `log8(len / 64 kB)`. Whenever [`MERGE_FACTOR`] segments share a tier they
/// are merged into one, so the segment count stays logarithmic in the number
/// of flushes. Every search walks every segment's dictionary (16 segments
/// took quiet search p50 from 1.2 to 4.8 ms), hence 4, not 8. Segments above
/// [`MERGE_MAX_BYTES`] (e.g. from `CREATE INDEX`) are left alone; merging
/// them inline would stall writers. REINDEX compacts everything.
const MERGE_FACTOR: usize = 4;
const TIER_BASE: u64 = 64 * 1024;
const MERGE_MAX_BYTES: u64 = 64 * 1024 * 1024;

fn tier(len: u64) -> u32 {
    let mut t = 0;
    let mut cap = TIER_BASE;
    while len > cap {
        cap *= 8;
        t += 1;
    }
    t
}

/// Merge full tiers of small segments, in `meta` (a copy the caller
/// installs later; merged-away chains go to `meta.retired`). Runs without
/// the metapage lock, under the flush mutex: VACUUM's pass 1 may clear
/// liveness bits of the inputs meanwhile, but the merged segment has a new
/// id, so that VACUUM's pass 2 (which waits for the mutex) vacuums it too.
unsafe fn merge_small_segments(index: pg_sys::Relation, meta: &mut Meta, written: &mut Written) {
    loop {
        let mut by_tier: std::collections::BTreeMap<u32, Vec<usize>> = Default::default();
        for (i, r) in meta.segments.iter().enumerate() {
            if r.len <= MERGE_MAX_BYTES {
                by_tier.entry(tier(r.len)).or_default().push(i);
            }
        }
        let Some(group) = by_tier.into_values().find(|v| v.len() >= MERGE_FACTOR) else {
            return;
        };
        let group: Vec<SegmentRef> = group.into_iter().take(MERGE_FACTOR).map(|i| meta.segments[i]).collect();

        let segs: Vec<tin_core::Segment> = group.iter().map(|r| scan::load_segment(index, r)).collect();
        let lives: Vec<Vec<u64>> = group
            .iter()
            .zip(&segs)
            .map(|(r, s)| storage::read_liveness(index, r, scan::live_words(s)))
            .collect();
        let inputs: Vec<(&tin_core::Segment, Option<&[u64]>)> =
            segs.iter().zip(&lives).map(|(s, l)| (s, Some(l.as_slice()))).collect();
        let merged = tin_core::Segment::merge(&inputs);

        let ids: HashSet<u32> = group.iter().map(|r| r.id).collect();
        meta.segments.retain(|r| !ids.contains(&r.id));
        for r in &group {
            meta.retired.push((r.first_block, r.n_blocks));
            meta.retired.push((r.live_first, r.live_blocks));
        }
        if let Some(seg) = merged {
            add_segment(index, meta, &seg, written);
        }
        debug_assert!(meta.fits());
    }
}

/// Free the pages of merged-away segments. Only called from VACUUM's
/// cleanup, after its own lock-free pass has finished, and under the flush
/// mutex (a flush may still be reading them).
unsafe fn free_retired(index: pg_sys::Relation) {
    let _mutex = FlushMutex::acquire(index, true);
    let lock = MetaLock::exclusive(index);
    let mut meta = lock.read();
    if meta.retired.is_empty() {
        return;
    }
    let blocks: Vec<u32> =
        meta.retired.iter().flat_map(|&(head, n)| storage::chain_blocks(index, head, n)).collect();
    let heads: Vec<u32> = meta.retired.iter().map(|&(head, _)| head).collect();
    meta.retired.clear();
    lock.write(index, &meta);
    drop(lock);
    // Before the pages can be reused: no backend may map an old copy under
    // a head block that a new segment then gets.
    crate::shared::forget(index, &heads);
    storage::free_pages(index, &blocks);
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
    // The flush mutex first: a flush in progress finishes (and its new
    // segments get vacuumed here), and none starts until we're done.
    let _mutex = FlushMutex::acquire(index, true);
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
    storage::free_pages(index, &old_pages);
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
    flush(index, true);
    free_retired(index);
    compact(index);
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
