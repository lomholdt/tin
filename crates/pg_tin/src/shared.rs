//! Segments shared by every backend, in dynamic shared memory.
//!
//! Without this, each backend decodes its own copy of every segment on its
//! first query: ~1 s and ~400 MB per backend at 5M rows, so 50 pooled
//! connections would hold 20 GB of identical copies. Instead the first
//! backend to need a segment copies it once into a DSM segment; the others
//! map it and parse it in place (`Segment::from_shared`), which copies only
//! the small page directory and docs bitmap.
//!
//! A registry in a named DSM segment (`GetNamedDSMSegment`, PG 17+) maps
//! (index relfile, segment's first block, segment id) to a DSM handle. The
//! id matters on hot standbys: WAL replay frees and reuses pages without
//! telling the registry, so a new segment can start at an old one's block;
//! ids are never reused within an index. Entries are
//! pinned, so a segment outlives the backend that loaded it, and unpinned
//! when VACUUM frees the segment's pages ([`forget`]) or, least recently
//! used first, when `tin.shared_cache_size` is exceeded. Unpinned memory
//! goes away once the last backend using it detaches.

use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::GucSetting;
use tin_core::bytes::{Backing, Bytes};
use tin_core::Segment;

use crate::storage::SegmentRef;

/// `tin.shared_cache_size` (MB): shared memory for segments; 0 disables
/// sharing (every backend decodes its own copies, as before).
pub static SHARED_CACHE_MB: GucSetting<i32> = GucSetting::<i32>::new(1024);

const MAX_ENTRIES: usize = 1024;

#[repr(C)]
#[derive(Copy, Clone)]
struct Entry {
    used: bool,
    spc: pg_sys::Oid,
    db: pg_sys::Oid,
    rel: pg_sys::RelFileNumber,
    head: u32,
    id: u32,
    len: u64,
    handle: pg_sys::dsm_handle,
    last_use: u64,
}

#[repr(C)]
struct Registry {
    lock: pg_sys::LWLock,
    tranche: i32,
    clock: u64,
    bytes: u64,
    entries: [Entry; MAX_ENTRIES],
}

#[pg_guard]
unsafe extern "C-unwind" fn init_registry(ptr: *mut std::ffi::c_void) {
    let reg = ptr as *mut Registry;
    std::ptr::write_bytes(reg, 0, 1);
    (*reg).tranche = pg_sys::LWLockNewTrancheId();
    pg_sys::LWLockInitialize(&mut (*reg).lock, (*reg).tranche);
}

thread_local! {
    static REGISTRY: std::cell::Cell<*mut Registry> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    /// Set once the backend is exiting: mappings are about to go.
    static EXITING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This backend's live mappings: Postgres allows one per segment, so a
    /// second user (VACUUM reading a segment a query has cached) shares it.
    static MAPPINGS: std::cell::RefCell<std::collections::HashMap<pg_sys::dsm_handle, std::sync::Weak<Mapped>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

unsafe fn registry() -> *mut Registry {
    REGISTRY.with(|r| {
        if r.get().is_null() {
            let mut found = false;
            let reg = pg_sys::GetNamedDSMSegment(
                // Versioned: a new layout must not read an old registry
                // left in a running server by the previous library.
                c"pg_tin segments v2".as_ptr(),
                std::mem::size_of::<Registry>(),
                Some(init_registry),
                &mut found,
            ) as *mut Registry;
            pg_sys::LWLockRegisterTranche((*reg).tranche, c"pg_tin_segments".as_ptr());
            // Detach our mappings before Postgres detaches every DSM segment
            // at exit, not after (from thread-local destructors).
            pg_sys::before_shmem_exit(Some(on_exit), pg_sys::Datum::from(0));
            r.set(reg);
        }
        r.get()
    })
}

#[pg_guard]
unsafe extern "C-unwind" fn on_exit(_code: std::ffi::c_int, _arg: pg_sys::Datum) {
    crate::scan::clear_cache();
    EXITING.with(|e| e.set(true));
}

/// A DSM segment mapped into this backend.
struct Mapped {
    seg: *mut pg_sys::dsm_segment,
    ptr: *const u8,
    len: usize,
}

// The mapping is read-only shared memory; it is only detached on the
// backend's own thread (`Segment`s never leave it).
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Backing for Mapped {
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        if !EXITING.with(|e| e.get()) {
            unsafe { pg_sys::dsm_detach(self.seg) };
        }
    }
}

unsafe fn map(seg: *mut pg_sys::dsm_segment, len: usize) -> Bytes {
    // Keep the mapping past the current resource owner (transaction).
    pg_sys::dsm_pin_mapping(seg);
    let ptr = pg_sys::dsm_segment_address(seg) as *const u8;
    let m = Arc::new(Mapped { seg, ptr, len });
    MAPPINGS.with(|ms| ms.borrow_mut().insert(pg_sys::dsm_segment_handle(seg), Arc::downgrade(&m)));
    Bytes::new(m)
}

/// This backend's existing mapping of `handle`, if any.
fn mapped(handle: pg_sys::dsm_handle) -> Option<Bytes> {
    MAPPINGS.with(|ms| {
        let mut ms = ms.borrow_mut();
        match ms.get(&handle).and_then(|w| w.upgrade()) {
            Some(m) => Some(Bytes::new(m)),
            None => {
                ms.remove(&handle);
                None
            }
        }
    })
}

/// An entry for the chain starting at `head` (whichever segment it holds).
fn same(e: &Entry, loc: &pg_sys::RelFileLocator, head: u32) -> bool {
    e.used && e.spc == loc.spcOid && e.db == loc.dbOid && e.rel == loc.relNumber && e.head == head
}

/// `r`'s segment, from shared memory if possible (loading it there if it
/// isn't yet), else decoded privately.
pub unsafe fn segment(index: pg_sys::Relation, r: &SegmentRef) -> Segment {
    let cap = SHARED_CACHE_MB.get() as u64 * 1024 * 1024;
    if cap == 0 || r.len > cap {
        return crate::scan::load_private(index, r);
    }
    let bytes = shared_bytes(index, r, cap);
    Segment::from_shared(bytes)
        .unwrap_or_else(|e| error!("tin: segment {} at block {} is corrupt: {e}", r.id, r.first_block))
}

unsafe fn shared_bytes(index: pg_sys::Relation, r: &SegmentRef, cap: u64) -> Bytes {
    let reg = registry();
    let loc = (*index).rd_locator;
    // Exclusive: a first load happens under it, so concurrent backends
    // wait for that copy instead of each making their own.
    pg_sys::LWLockAcquire(&mut (*reg).lock, pg_sys::LWLockMode::LW_EXCLUSIVE);
    (*reg).clock += 1;
    let now = (*reg).clock;
    let entries = &mut (*reg).entries;
    if let Some(e) = entries.iter_mut().find(|e| same(e, &loc, r.first_block)) {
        // Another segment at the same block (replay reused its pages) is stale.
        if e.id == r.id && e.len == r.len {
            if let Some(b) = mapped(e.handle) {
                e.last_use = now;
                pg_sys::LWLockRelease(&mut (*reg).lock);
                return b;
            }
            let seg = pg_sys::dsm_attach(e.handle);
            if !seg.is_null() {
                // Guard against a stale entry: a dropped index's relfile
                // number can come back for a new index. The first page must
                // match what we mapped.
                let prefix = crate::storage::read_blob_prefix(index, r.first_block);
                let mapped =
                    std::slice::from_raw_parts(pg_sys::dsm_segment_address(seg) as *const u8, prefix.len());
                if prefix == mapped {
                    e.last_use = now;
                    pg_sys::LWLockRelease(&mut (*reg).lock);
                    return map(seg, r.len as usize);
                }
                pg_sys::dsm_detach(seg);
            }
        }
        // Stale: drop it and load afresh.
        pg_sys::dsm_unpin_segment(e.handle);
        (*reg).bytes -= e.len;
        e.used = false;
    }
    // Make room: least recently used first.
    while (*reg).bytes + r.len > cap {
        let Some(victim) = entries.iter_mut().filter(|e| e.used).min_by_key(|e| e.last_use) else { break };
        pg_sys::dsm_unpin_segment(victim.handle);
        (*reg).bytes -= victim.len;
        victim.used = false;
    }
    let slot = entries.iter().position(|e| !e.used);
    let seg = match slot {
        Some(_) => pg_sys::dsm_create(r.len as usize, pg_sys::DSM_CREATE_NULL_IF_MAXSEGMENTS as i32),
        None => std::ptr::null_mut(),
    };
    if seg.is_null() {
        pg_sys::LWLockRelease(&mut (*reg).lock);
        return Bytes::from_vec(crate::storage::read_blob(index, r.first_block, r.n_blocks, r.len));
    }
    let out = std::slice::from_raw_parts_mut(pg_sys::dsm_segment_address(seg) as *mut u8, r.len as usize);
    crate::storage::read_blob_into(index, r.first_block, r.n_blocks, out);
    pg_sys::dsm_pin_segment(seg);
    entries[slot.unwrap()] = Entry {
        used: true,
        spc: loc.spcOid,
        db: loc.dbOid,
        rel: loc.relNumber,
        head: r.first_block,
        id: r.id,
        len: r.len,
        handle: pg_sys::dsm_segment_handle(seg),
        last_use: now,
    };
    (*reg).bytes += r.len;
    pg_sys::LWLockRelease(&mut (*reg).lock);
    map(seg, r.len as usize)
}

/// Put a just-installed segment (segment `id`, its serialized `bytes`,
/// whose chain starts at `head`) into shared memory, so readers map it
/// instead of copying it in. Best effort: skipped if it doesn't fit.
pub unsafe fn publish(index: pg_sys::Relation, head: u32, id: u32, bytes: &[u8]) {
    let cap = SHARED_CACHE_MB.get() as u64 * 1024 * 1024;
    let len = bytes.len() as u64;
    if cap == 0 || len > cap {
        return;
    }
    let reg = registry();
    let loc = (*index).rd_locator;
    pg_sys::LWLockAcquire(&mut (*reg).lock, pg_sys::LWLockMode::LW_EXCLUSIVE);
    let entries = &mut (*reg).entries;
    if let Some(e) = entries.iter_mut().find(|e| same(e, &loc, head)) {
        if e.id == id {
            pg_sys::LWLockRelease(&mut (*reg).lock);
            return;
        }
        pg_sys::dsm_unpin_segment(e.handle);
        (*reg).bytes -= e.len;
        e.used = false;
    }
    (*reg).clock += 1;
    let now = (*reg).clock;
    while (*reg).bytes + len > cap {
        let Some(victim) = entries.iter_mut().filter(|e| e.used).min_by_key(|e| e.last_use) else { break };
        pg_sys::dsm_unpin_segment(victim.handle);
        (*reg).bytes -= victim.len;
        victim.used = false;
    }
    let Some(slot) = entries.iter().position(|e| !e.used) else {
        pg_sys::LWLockRelease(&mut (*reg).lock);
        return;
    };
    let seg = pg_sys::dsm_create(bytes.len(), pg_sys::DSM_CREATE_NULL_IF_MAXSEGMENTS as i32);
    if seg.is_null() {
        pg_sys::LWLockRelease(&mut (*reg).lock);
        return;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), pg_sys::dsm_segment_address(seg) as *mut u8, bytes.len());
    pg_sys::dsm_pin_segment(seg);
    entries[slot] = Entry {
        used: true,
        spc: loc.spcOid,
        db: loc.dbOid,
        rel: loc.relNumber,
        head,
        id,
        len,
        handle: pg_sys::dsm_segment_handle(seg),
        last_use: now,
    };
    (*reg).bytes += len;
    pg_sys::LWLockRelease(&mut (*reg).lock);
    // The pin keeps it alive; this backend doesn't need its own mapping.
    pg_sys::dsm_detach(seg);
}

/// The segments whose chains start at `heads` are being freed: drop their
/// shared copies (memory is released once no backend maps them).
pub unsafe fn forget(index: pg_sys::Relation, heads: &[u32]) {
    if heads.is_empty() {
        return;
    }
    let reg = registry();
    let loc = (*index).rd_locator;
    pg_sys::LWLockAcquire(&mut (*reg).lock, pg_sys::LWLockMode::LW_EXCLUSIVE);
    for e in (*reg).entries.iter_mut() {
        if heads.iter().any(|&h| same(e, &loc, h)) {
            pg_sys::dsm_unpin_segment(e.handle);
            (*reg).bytes -= e.len;
            e.used = false;
        }
    }
    pg_sys::LWLockRelease(&mut (*reg).lock);
}

/// (entries, bytes) in the registry, for `tin_shared_stats()`.
pub unsafe fn stats() -> (i64, i64) {
    let reg = registry();
    pg_sys::LWLockAcquire(&mut (*reg).lock, pg_sys::LWLockMode::LW_SHARED);
    let n = (*reg).entries.iter().filter(|e| e.used).count() as i64;
    let b = (*reg).bytes as i64;
    pg_sys::LWLockRelease(&mut (*reg).lock);
    (n, b)
}
