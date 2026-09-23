//! Index relation layout (format 3).
//!
//! ```text
//! block 0   metapage: generation, segment table, pending-list pointers,
//!           retired page chains
//! blobs     every other page belongs to a chain: [next u32][flags u32][data…]
//!   segment   Segment::to_bytes() split across a chain (8160 data bytes/page)
//!   liveness  per segment: its liveness bitmap (u64 LE words) on a chain
//!   pending   records of tuples inserted since the last flush
//! ```
//!
//! Chains let any freed page be reused for anything: pages come from the free
//! space map when possible, otherwise the relation is extended. A freed page
//! is first stamped (WAL-logged) so a stale FSM entry can never hand out a
//! page still in use.
//!
//! Every page is a standard Postgres page (payload below `pd_lower`). The
//! build writes unlogged and WAL-logs the whole relation once at the end
//! (`log_newpage_range`); every later change goes through Generic WAL.
//!
//! Concurrency: structural changes (pending appends, flushes, merges, the
//! last phase of VACUUM) hold the metapage buffer lock exclusively. Readers
//! hold it shared while they read the metapage, liveness, the pending list
//! and any newly needed segment. Merged-away segments are *retired* rather
//! than freed and only recycled by the next VACUUM's cleanup, because
//! VACUUM's first pass reads segments without the metapage lock.

use pgrx::pg_sys;

const META_MAGIC: u32 = u32::from_le_bytes(*b"TIN1");
const META_VERSION: u32 = 3;
/// `SizeOfPageHeaderData`, already MAXALIGNed.
const PAGE_HEADER: usize = 24;
const BLCKSZ: usize = pg_sys::BLCKSZ as usize;
pub const PAGE_PAYLOAD: usize = BLCKSZ - PAGE_HEADER;
const META_HEADER: usize = 64;
const META_ENTRY: usize = 32;
/// Segments that fit in the metapage's table.
pub const MAX_SEGMENTS: usize = (PAGE_PAYLOAD - META_HEADER) / META_ENTRY;
/// Chain page header: next block, flags.
const CHAIN_HEADER: usize = 8;
const CHAIN_CAP: usize = PAGE_PAYLOAD - CHAIN_HEADER;
const PENDING_HEADER: usize = CHAIN_HEADER;
const PENDING_CAP: usize = CHAIN_CAP;
const INVALID_BLOCK: u32 = u32::MAX;
/// Flags word of a pending page that has been freed.
const FREED: u32 = u32::from_le_bytes(*b"FREE");

/// Where one segment and its liveness bitmap live.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SegmentRef {
    pub id: u32,
    pub first_block: u32,
    pub n_blocks: u32,
    pub len: u64,
    pub live_first: u32,
    pub live_blocks: u32,
}

/// Decoded metapage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Bumped by every change other than a plain pending append: flushes and
    /// VACUUM. Backends reload liveness and the pending list when it moves.
    pub generation: u64,
    pub next_segment_id: u32,
    pub pending_head: u32,
    pub pending_tail: u32,
    /// Stream bytes used in the tail page.
    pub pending_tail_used: u32,
    /// Total pending stream bytes; always on a record boundary.
    pub pending_bytes: u64,
    pub pending_count: u64,
    pub segments: Vec<SegmentRef>,
    /// Chains (head, pages) of merged-away segments and their liveness.
    /// Freed by the next VACUUM, so a concurrent VACUUM's lock-free pass
    /// never reads a recycled page.
    pub retired: Vec<(u32, u32)>,
}

impl Meta {
    pub fn empty() -> Meta {
        Meta {
            generation: 1,
            next_segment_id: 0,
            pending_head: INVALID_BLOCK,
            pending_tail: INVALID_BLOCK,
            pending_tail_used: 0,
            pending_bytes: 0,
            pending_count: 0,
            segments: Vec::new(),
            retired: Vec::new(),
        }
    }

    /// Whether one more segment entry fits in the metapage.
    pub fn can_add_segment(&self) -> bool {
        META_HEADER + (self.segments.len() + 1) * META_ENTRY + self.retired.len() * 8 <= PAGE_PAYLOAD
    }

    /// Whether the encoded metapage fits in one page.
    pub fn fits(&self) -> bool {
        META_HEADER + self.segments.len() * META_ENTRY + self.retired.len() * 8 <= PAGE_PAYLOAD
    }

    fn encode(&self) -> Vec<u8> {
        assert!(self.fits(), "metapage overflow");
        let mut out = Vec::with_capacity(META_HEADER + self.segments.len() * META_ENTRY);
        out.extend_from_slice(&META_MAGIC.to_le_bytes());
        out.extend_from_slice(&META_VERSION.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        for v in [
            self.next_segment_id,
            self.segments.len() as u32,
            self.pending_head,
            self.pending_tail,
            self.pending_tail_used,
            self.retired.len() as u32,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.pending_bytes.to_le_bytes());
        out.extend_from_slice(&self.pending_count.to_le_bytes());
        out.resize(META_HEADER, 0);
        for s in &self.segments {
            for v in [s.id, s.first_block, s.n_blocks, s.live_first, s.live_blocks, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&s.len.to_le_bytes());
        }
        for (first, n) in &self.retired {
            out.extend_from_slice(&first.to_le_bytes());
            out.extend_from_slice(&n.to_le_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Meta {
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if b.len() < 8 || u32_at(0) != META_MAGIC {
            pgrx::error!("tin: index metapage is missing or corrupt");
        }
        if u32_at(4) != META_VERSION {
            pgrx::error!(
                "tin: index has on-disk format {}, this version needs {META_VERSION}; REINDEX it",
                u32_at(4)
            );
        }
        let (n, n_retired) =
            if b.len() >= META_HEADER { (u32_at(20) as usize, u32_at(36) as usize) } else { (usize::MAX, 0) };
        if n == usize::MAX || b.len() < META_HEADER + n * META_ENTRY + n_retired * 8 {
            pgrx::error!("tin: index metapage is truncated");
        }
        let segments = (0..n)
            .map(|i| {
                let at = META_HEADER + i * META_ENTRY;
                SegmentRef {
                    id: u32_at(at),
                    first_block: u32_at(at + 4),
                    n_blocks: u32_at(at + 8),
                    live_first: u32_at(at + 12),
                    live_blocks: u32_at(at + 16),
                    len: u64_at(at + 24),
                }
            })
            .collect();
        let retired_at = META_HEADER + n * META_ENTRY;
        let retired =
            (0..n_retired).map(|i| (u32_at(retired_at + i * 8), u32_at(retired_at + i * 8 + 4))).collect();
        Meta {
            retired,
            generation: u64_at(8),
            next_segment_id: u32_at(16),
            pending_head: u32_at(24),
            pending_tail: u32_at(28),
            pending_tail_used: u32_at(32),
            pending_bytes: u64_at(40),
            pending_count: u64_at(48),
            segments,
        }
    }
}

// --- Pages -------------------------------------------------------------------

unsafe fn payload_ptr(page: pg_sys::Page) -> *mut u8 {
    (page as *mut u8).add(PAGE_HEADER)
}

unsafe fn payload_len(page: pg_sys::Page) -> usize {
    let lower = (*(page as *const pg_sys::PageHeaderData)).pd_lower as usize;
    if !(PAGE_HEADER..=BLCKSZ).contains(&lower) {
        pgrx::error!("tin: index page is corrupt (pd_lower {lower})");
    }
    lower - PAGE_HEADER
}

/// Initialize `page` with `payload`.
unsafe fn init_page(page: pg_sys::Page, payload: &[u8]) {
    assert!(payload.len() <= PAGE_PAYLOAD);
    pg_sys::PageInit(page, BLCKSZ, 0);
    std::ptr::copy_nonoverlapping(payload.as_ptr(), payload_ptr(page), payload.len());
    (*(page as *mut pg_sys::PageHeaderData)).pd_lower = (PAGE_HEADER + payload.len()) as u16;
}

/// Overwrite `bytes` at `offset` of an initialized page's payload, growing
/// `pd_lower` if needed.
unsafe fn write_at(page: pg_sys::Page, offset: usize, bytes: &[u8]) {
    assert!(offset + bytes.len() <= PAGE_PAYLOAD);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), payload_ptr(page).add(offset), bytes.len());
    let hdr = page as *mut pg_sys::PageHeaderData;
    let end = (PAGE_HEADER + offset + bytes.len()) as u16;
    if (*hdr).pd_lower < end {
        (*hdr).pd_lower = end;
    }
}

unsafe fn u32_in_payload(page: pg_sys::Page, offset: usize) -> u32 {
    u32::from_le_bytes(std::slice::from_raw_parts(payload_ptr(page).add(offset), 4).try_into().unwrap())
}

/// A pinned, locked buffer, released on drop.
pub struct Locked(pub pg_sys::Buffer);

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe { pg_sys::UnlockReleaseBuffer(self.0) };
    }
}

unsafe fn read_locked(index: pg_sys::Relation, blk: u32, mode: u32) -> Locked {
    let buf = pg_sys::ReadBufferExtended(
        index,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        blk,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    pg_sys::LockBuffer(buf, mode as i32);
    Locked(buf)
}

/// Extend `fork` by one page; returned pinned and exclusively locked.
unsafe fn extend(index: pg_sys::Relation, fork: pg_sys::ForkNumber::Type) -> Locked {
    let bmr = pg_sys::BufferManagerRelation { rel: index, smgr: std::ptr::null_mut(), relpersistence: 0 };
    let mut flags = pg_sys::ExtendBufferedFlags::EB_LOCK_FIRST;
    if fork == pg_sys::ForkNumber::INIT_FORKNUM {
        // Nobody else can see the init fork while it is being created.
        flags |= pg_sys::ExtendBufferedFlags::EB_SKIP_EXTENSION_LOCK;
    }
    Locked(pg_sys::ExtendBufferedRel(bmr, fork, std::ptr::null_mut(), flags))
}

/// A page for a chain: a freed page from the free space map if one checks
/// out, otherwise a new one. Exclusively locked, contents undefined.
unsafe fn alloc_page(index: pg_sys::Relation) -> Locked {
    loop {
        let blk = pg_sys::GetFreeIndexPage(index);
        if blk == INVALID_BLOCK {
            return extend(index, pg_sys::ForkNumber::MAIN_FORKNUM);
        }
        let page = read_locked(index, blk, pg_sys::BUFFER_LOCK_EXCLUSIVE);
        let p = pg_sys::BufferGetPage(page.0);
        // The FSM isn't WAL-logged and can be stale after a crash: only
        // recycle pages we stamped as freed.
        if payload_len(p) >= PENDING_HEADER && u32_in_payload(p, 4) == FREED {
            return page;
        }
    }
}

/// Apply `f` to the pages of `bufs` inside one Generic WAL record. `full`
/// marks buffers whose previous contents don't matter (new pages).
unsafe fn logged(index: pg_sys::Relation, bufs: &[(&Locked, bool)], f: impl FnOnce(&[pg_sys::Page])) {
    assert!(bufs.len() <= 4, "generic WAL records hold at most 4 pages");
    let state = pg_sys::GenericXLogStart(index);
    let pages: Vec<pg_sys::Page> = bufs
        .iter()
        .map(|(b, full)| {
            let flags = if *full { pg_sys::GENERIC_XLOG_FULL_IMAGE as i32 } else { 0 };
            pg_sys::GenericXLogRegisterBuffer(state, b.0, flags)
        })
        .collect();
    f(&pages);
    pg_sys::GenericXLogFinish(state);
}

// --- Metapage ----------------------------------------------------------------

/// The locked metapage.
pub struct MetaLock(Locked);

impl MetaLock {
    pub unsafe fn share(index: pg_sys::Relation) -> MetaLock {
        MetaLock(read_locked(index, 0, pg_sys::BUFFER_LOCK_SHARE))
    }

    pub unsafe fn exclusive(index: pg_sys::Relation) -> MetaLock {
        MetaLock(read_locked(index, 0, pg_sys::BUFFER_LOCK_EXCLUSIVE))
    }

    pub unsafe fn read(&self) -> Meta {
        let page = pg_sys::BufferGetPage(self.0 .0);
        Meta::decode(std::slice::from_raw_parts(payload_ptr(page), payload_len(page)))
    }

    /// WAL-logged write; only valid on an exclusive lock.
    pub unsafe fn write(&self, index: pg_sys::Relation, meta: &Meta) {
        let bytes = meta.encode();
        logged(index, &[(&self.0, false)], |p| init_page(p[0], &bytes));
    }
}

// --- Build-time writes (unlogged; `log_all_pages` at the end) -------------------

/// Reserve block 0 for the metapage. Must be the first page written.
pub unsafe fn init_metapage(index: pg_sys::Relation) {
    let buf = extend(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    assert_eq!(pg_sys::BufferGetBlockNumber(buf.0), 0, "metapage must be block 0");
    init_page(pg_sys::BufferGetPage(buf.0), &Meta::empty().encode());
    pg_sys::MarkBufferDirty(buf.0);
}

/// Write `bytes` as a new page chain; returns (head block, pages).
/// `wal`: log each page now (runtime) or leave it to `log_all_pages` (build).
pub unsafe fn write_blob(index: pg_sys::Relation, bytes: &[u8], wal: bool) -> (u32, u32) {
    let mut head = INVALID_BLOCK;
    let mut n = 0u32;
    let mut prev: Option<Locked> = None;
    for chunk in bytes.chunks(CHAIN_CAP) {
        let page = alloc_page(index);
        let blk = pg_sys::BufferGetBlockNumber(page.0);
        let mut payload = chain_header(INVALID_BLOCK, 0).to_vec();
        payload.extend_from_slice(chunk);
        if wal {
            match &prev {
                Some(p) => logged(index, &[(p, false), (&page, true)], |pp| {
                    write_at(pp[0], 0, &blk.to_le_bytes());
                    init_page(pp[1], &payload);
                }),
                None => logged(index, &[(&page, true)], |pp| init_page(pp[0], &payload)),
            }
        } else {
            if let Some(p) = &prev {
                write_at(pg_sys::BufferGetPage(p.0), 0, &blk.to_le_bytes());
                pg_sys::MarkBufferDirty(p.0);
            }
            init_page(pg_sys::BufferGetPage(page.0), &payload);
            pg_sys::MarkBufferDirty(page.0);
        }
        if prev.is_none() {
            head = blk;
        }
        prev = Some(page);
        n += 1;
    }
    (head, n)
}

/// Blocks of the `n`-page chain starting at `head`.
pub unsafe fn chain_blocks(index: pg_sys::Relation, head: u32, n: u32) -> Vec<u32> {
    let mut blocks = Vec::with_capacity(n as usize);
    let mut blk = head;
    for _ in 0..n {
        blocks.push(blk);
        let buf = read_locked(index, blk, pg_sys::BUFFER_LOCK_SHARE);
        blk = u32_in_payload(pg_sys::BufferGetPage(buf.0), 0);
    }
    blocks
}

/// Write the metapage during a build (no WAL).
pub unsafe fn write_meta_unlogged(index: pg_sys::Relation, meta: &Meta) {
    let buf = read_locked(index, 0, pg_sys::BUFFER_LOCK_EXCLUSIVE);
    init_page(pg_sys::BufferGetPage(buf.0), &meta.encode());
    pg_sys::MarkBufferDirty(buf.0);
}

/// WAL-log every page of the main fork (full-page images), if the relation
/// is WAL-logged at all.
pub unsafe fn log_all_pages(index: pg_sys::Relation) {
    if (*(*index).rd_rel).relpersistence != pg_sys::RELPERSISTENCE_PERMANENT as core::ffi::c_char {
        return;
    }
    let n = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    pg_sys::log_newpage_range(index, pg_sys::ForkNumber::MAIN_FORKNUM, 0, n, true);
}

/// An empty metapage in the init fork, for unlogged tables.
pub unsafe fn write_empty_init_fork(index: pg_sys::Relation) {
    let buf = extend(index, pg_sys::ForkNumber::INIT_FORKNUM);
    init_page(pg_sys::BufferGetPage(buf.0), &Meta::empty().encode());
    pg_sys::MarkBufferDirty(buf.0);
    pg_sys::log_newpage_buffer(buf.0, true);
}

// --- Segments and liveness ---------------------------------------------------------

/// The data of the `n`-page chain starting at `head` (`len` bytes).
pub unsafe fn read_blob(index: pg_sys::Relation, head: u32, n: u32, len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut blk = head;
    for _ in 0..n {
        let buf = read_locked(index, blk, pg_sys::BUFFER_LOCK_SHARE);
        let page = pg_sys::BufferGetPage(buf.0);
        let data = payload_len(page).saturating_sub(CHAIN_HEADER);
        out.extend_from_slice(std::slice::from_raw_parts(payload_ptr(page).add(CHAIN_HEADER), data));
        blk = u32_in_payload(page, 0);
    }
    if out.len() as u64 != len {
        pgrx::error!("tin: page chain at block {head} holds {} bytes, expected {len}", out.len());
    }
    out
}

pub fn words_to_bytes(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A segment's liveness bitmap (`words` long).
pub unsafe fn read_liveness(index: pg_sys::Relation, seg: &SegmentRef, words: usize) -> Vec<u64> {
    let bytes = read_blob(index, seg.live_first, seg.live_blocks, (words * 8) as u64);
    bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

/// Rewrite the pages of `seg`'s liveness chain whose words differ between
/// `old` and `new`, one Generic WAL record each.
pub unsafe fn update_liveness(index: pg_sys::Relation, seg: &SegmentRef, old: &[u64], new: &[u64]) {
    const WORDS_PER_PAGE: usize = CHAIN_CAP / 8;
    let mut blk = seg.live_first;
    for (o, n) in old.chunks(WORDS_PER_PAGE).zip(new.chunks(WORDS_PER_PAGE)) {
        let buf = read_locked(index, blk, pg_sys::BUFFER_LOCK_EXCLUSIVE);
        let next = u32_in_payload(pg_sys::BufferGetPage(buf.0), 0);
        if o != n {
            let bytes = words_to_bytes(n);
            logged(index, &[(&buf, false)], |p| write_at(p[0], CHAIN_HEADER, &bytes));
        }
        blk = next;
    }
}

// --- Pending list ----------------------------------------------------------------

/// Where an incremental reader of the pending stream stopped.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingPos {
    pub block: u32,
    pub used: u32,
    pub total: u64,
}

impl PendingPos {
    pub fn start(meta: &Meta) -> PendingPos {
        PendingPos { block: meta.pending_head, used: 0, total: 0 }
    }
}

fn chain_header(next: u32, flags: u32) -> [u8; CHAIN_HEADER] {
    let mut h = [0u8; CHAIN_HEADER];
    h[..4].copy_from_slice(&next.to_le_bytes());
    h[4..].copy_from_slice(&flags.to_le_bytes());
    h
}

/// Pending stream bytes from `from` up to `meta.pending_bytes`. Caller holds
/// the metapage lock (shared or exclusive) that `meta` was read under.
pub unsafe fn read_pending(index: pg_sys::Relation, meta: &Meta, from: PendingPos) -> (Vec<u8>, PendingPos) {
    let mut out = Vec::with_capacity(meta.pending_bytes.saturating_sub(from.total) as usize);
    // Nothing read yet (possibly while the list was still empty): start at
    // the current head.
    let mut pos = if from.total == 0 { PendingPos::start(meta) } else { from };
    while pos.total < meta.pending_bytes {
        if pos.used as usize == PENDING_CAP {
            let buf = read_locked(index, pos.block, pg_sys::BUFFER_LOCK_SHARE);
            pos = PendingPos {
                block: u32_in_payload(pg_sys::BufferGetPage(buf.0), 0),
                used: 0,
                total: pos.total,
            };
            continue;
        }
        let buf = read_locked(index, pos.block, pg_sys::BUFFER_LOCK_SHARE);
        let page = pg_sys::BufferGetPage(buf.0);
        let take = ((PENDING_CAP - pos.used as usize) as u64).min(meta.pending_bytes - pos.total) as usize;
        let data = payload_ptr(page).add(PENDING_HEADER + pos.used as usize);
        out.extend_from_slice(std::slice::from_raw_parts(data, take));
        pos.used += take as u32;
        pos.total += take as u64;
    }
    (out, pos)
}

/// Blocks of the pending chain, head to tail.
pub unsafe fn pending_blocks(index: pg_sys::Relation, meta: &Meta) -> Vec<u32> {
    let mut blocks = Vec::new();
    let mut blk = meta.pending_head;
    while blk != INVALID_BLOCK {
        blocks.push(blk);
        if blk == meta.pending_tail {
            break;
        }
        let buf = read_locked(index, blk, pg_sys::BUFFER_LOCK_SHARE);
        blk = u32_in_payload(pg_sys::BufferGetPage(buf.0), 0);
    }
    blocks
}

/// Append one encoded record to the pending list and update `meta`, which
/// is written to the metapage in the same WAL record as the record's last
/// chunk. Caller holds `lock` (the metapage) exclusively.
pub unsafe fn append_pending(index: pg_sys::Relation, lock: &MetaLock, meta: &mut Meta, record: &[u8]) {
    if meta.pending_head == INVALID_BLOCK {
        let page = alloc_page(index);
        let blk = pg_sys::BufferGetBlockNumber(page.0);
        logged(index, &[(&page, true)], |p| init_page(p[0], &chain_header(INVALID_BLOCK, 0)));
        meta.pending_head = blk;
        meta.pending_tail = blk;
        meta.pending_tail_used = 0;
    }
    let mut rest = record;
    loop {
        if meta.pending_tail_used as usize == PENDING_CAP {
            // Tail is full: link a fresh page. (A crash after this but before
            // the metapage update only leaks the new page.)
            let tail = read_locked(index, meta.pending_tail, pg_sys::BUFFER_LOCK_EXCLUSIVE);
            let page = alloc_page(index);
            let blk = pg_sys::BufferGetBlockNumber(page.0);
            logged(index, &[(&tail, false), (&page, true)], |p| {
                write_at(p[0], 0, &blk.to_le_bytes());
                init_page(p[1], &chain_header(INVALID_BLOCK, 0));
            });
            meta.pending_tail = blk;
            meta.pending_tail_used = 0;
        }
        let take = rest.len().min(PENDING_CAP - meta.pending_tail_used as usize);
        let (chunk, after) = rest.split_at(take);
        let offset = PENDING_HEADER + meta.pending_tail_used as usize;
        meta.pending_tail_used += take as u32;
        let tail = read_locked(index, meta.pending_tail, pg_sys::BUFFER_LOCK_EXCLUSIVE);
        if after.is_empty() {
            meta.pending_bytes += record.len() as u64;
            meta.pending_count += 1;
            let bytes = meta.encode();
            logged(index, &[(&tail, false), (&lock.0, false)], |p| {
                write_at(p[0], offset, chunk);
                init_page(p[1], &bytes);
            });
            return;
        }
        logged(index, &[(&tail, false)], |p| write_at(p[0], offset, chunk));
        rest = after;
    }
}

/// Point `meta` at a fresh pending chain holding `stream` (`count` records),
/// or at none. Caller then writes `meta` and frees the old chain.
pub unsafe fn rewrite_pending(index: pg_sys::Relation, meta: &mut Meta, stream: &[u8], count: u64) {
    meta.pending_head = INVALID_BLOCK;
    meta.pending_tail = INVALID_BLOCK;
    meta.pending_tail_used = 0;
    meta.pending_bytes = 0;
    meta.pending_count = 0;
    let mut prev: Option<Locked> = None;
    for chunk in stream.chunks(PENDING_CAP) {
        let page = alloc_page(index);
        let blk = pg_sys::BufferGetBlockNumber(page.0);
        let mut payload = chain_header(INVALID_BLOCK, 0).to_vec();
        payload.extend_from_slice(chunk);
        match &prev {
            Some(p) => logged(index, &[(p, false), (&page, true)], |pp| {
                write_at(pp[0], 0, &blk.to_le_bytes());
                init_page(pp[1], &payload);
            }),
            None => {
                logged(index, &[(&page, true)], |pp| init_page(pp[0], &payload));
                meta.pending_head = blk;
            }
        }
        meta.pending_tail = blk;
        meta.pending_tail_used = chunk.len() as u32;
        prev = Some(page);
    }
    meta.pending_bytes = stream.len() as u64;
    meta.pending_count = if stream.is_empty() { 0 } else { count };
}

/// Stamp pages as freed (WAL-logged) and hand them to the free space map.
/// Only pending pages are ever taken back out (see `alloc_pending_page`).
pub unsafe fn free_pages(index: pg_sys::Relation, blocks: &[u32]) {
    for chunk in blocks.chunks(4) {
        let bufs: Vec<Locked> =
            chunk.iter().map(|&b| read_locked(index, b, pg_sys::BUFFER_LOCK_EXCLUSIVE)).collect();
        let regs: Vec<(&Locked, bool)> = bufs.iter().map(|b| (b, true)).collect();
        logged(index, &regs, |pages| {
            for &p in pages {
                init_page(p, &chain_header(INVALID_BLOCK, FREED));
            }
        });
    }
    for &b in blocks {
        pg_sys::RecordFreeIndexPage(index, b);
    }
}
