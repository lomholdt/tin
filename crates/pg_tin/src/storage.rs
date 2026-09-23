//! Index relation layout.
//!
//! ```text
//! block 0      metapage: magic, version, segment table
//! block 1..    segments, each serialized with `Segment::to_bytes` and laid
//!              across consecutive pages (8168 payload bytes per page)
//! ```
//!
//! Pages are ordinary Postgres pages (standard header, payload below
//! `pd_lower`), so `log_newpage_range` can WAL-log them with the hole
//! compressed away. The index is immutable after the build in Phase 1.

use pgrx::pg_sys;
use tin_core::{Index, Segment};

const META_MAGIC: u32 = u32::from_le_bytes(*b"TIN1");
const META_VERSION: u32 = 1;
/// `SizeOfPageHeaderData`, already MAXALIGNed.
const PAGE_HEADER: usize = 24;
const BLCKSZ: usize = pg_sys::BLCKSZ as usize;
pub const PAGE_PAYLOAD: usize = BLCKSZ - PAGE_HEADER;
const META_HEADER: usize = 16;
const META_ENTRY: usize = 16;
/// Segments that fit in the metapage's table.
pub const MAX_SEGMENTS: usize = (PAGE_PAYLOAD - META_HEADER) / META_ENTRY;

/// Where one serialized segment lives.
#[derive(Copy, Clone, Debug)]
pub struct SegmentRef {
    pub first_block: u32,
    pub n_blocks: u32,
    pub len: u64,
}

fn encode_meta(segments: &[SegmentRef]) -> Vec<u8> {
    assert!(segments.len() <= MAX_SEGMENTS, "too many segments for the metapage");
    let mut out = Vec::with_capacity(META_HEADER + segments.len() * META_ENTRY);
    out.extend_from_slice(&META_MAGIC.to_le_bytes());
    out.extend_from_slice(&META_VERSION.to_le_bytes());
    out.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for s in segments {
        out.extend_from_slice(&s.first_block.to_le_bytes());
        out.extend_from_slice(&s.n_blocks.to_le_bytes());
        out.extend_from_slice(&s.len.to_le_bytes());
    }
    out
}

fn decode_meta(b: &[u8]) -> Vec<SegmentRef> {
    let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    if b.len() < META_HEADER || u32_at(0) != META_MAGIC {
        pgrx::error!("tin: index metapage is missing or corrupt");
    }
    if u32_at(4) != META_VERSION {
        pgrx::error!("tin: unsupported index version {}; REINDEX it", u32_at(4));
    }
    let n = u32_at(8) as usize;
    if b.len() < META_HEADER + n * META_ENTRY {
        pgrx::error!("tin: index metapage is truncated");
    }
    (0..n)
        .map(|i| {
            let at = META_HEADER + i * META_ENTRY;
            SegmentRef {
                first_block: u32_at(at),
                n_blocks: u32_at(at + 4),
                len: u64::from_le_bytes(b[at + 8..at + 16].try_into().unwrap()),
            }
        })
        .collect()
}

/// Extend `fork` by one page and return it pinned and exclusively locked.
unsafe fn new_buffer(index: pg_sys::Relation, fork: pg_sys::ForkNumber::Type) -> pg_sys::Buffer {
    let bmr = pg_sys::BufferManagerRelation { rel: index, smgr: std::ptr::null_mut(), relpersistence: 0 };
    let mut flags = pg_sys::ExtendBufferedFlags::EB_LOCK_FIRST;
    if fork == pg_sys::ForkNumber::INIT_FORKNUM {
        // Nobody else can see the init fork while it is being created.
        flags |= pg_sys::ExtendBufferedFlags::EB_SKIP_EXTENSION_LOCK;
    }
    pg_sys::ExtendBufferedRel(bmr, fork, std::ptr::null_mut(), flags)
}

/// Initialize the locked buffer's page with `payload` and mark it dirty.
/// Does not WAL-log; callers log the whole range afterwards.
unsafe fn fill_page(buf: pg_sys::Buffer, payload: &[u8]) {
    assert!(payload.len() <= PAGE_PAYLOAD);
    let page = pg_sys::BufferGetPage(buf);
    pg_sys::PageInit(page, BLCKSZ, 0);
    std::ptr::copy_nonoverlapping(payload.as_ptr(), (page as *mut u8).add(PAGE_HEADER), payload.len());
    (*(page as *mut pg_sys::PageHeaderData)).pd_lower = (PAGE_HEADER + payload.len()) as u16;
    pg_sys::MarkBufferDirty(buf);
}

/// Reserve block 0 for the metapage. Must be the first page written.
pub unsafe fn init_metapage(index: pg_sys::Relation) {
    let buf = new_buffer(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    assert_eq!(pg_sys::BufferGetBlockNumber(buf), 0, "metapage must be block 0");
    fill_page(buf, &encode_meta(&[]));
    pg_sys::UnlockReleaseBuffer(buf);
}

/// Append a serialized segment as consecutive pages.
pub unsafe fn write_segment(index: pg_sys::Relation, bytes: &[u8]) -> SegmentRef {
    let mut first_block = None;
    let mut n_blocks = 0;
    for chunk in bytes.chunks(PAGE_PAYLOAD) {
        let buf = new_buffer(index, pg_sys::ForkNumber::MAIN_FORKNUM);
        let blk = pg_sys::BufferGetBlockNumber(buf);
        if let Some(first) = first_block {
            assert_eq!(blk, first + n_blocks, "segment pages must be consecutive");
        } else {
            first_block = Some(blk);
        }
        fill_page(buf, chunk);
        pg_sys::UnlockReleaseBuffer(buf);
        n_blocks += 1;
    }
    SegmentRef {
        first_block: first_block.expect("segments are never empty"),
        n_blocks,
        len: bytes.len() as u64,
    }
}

/// Write the segment table into the metapage.
pub unsafe fn write_metapage(index: pg_sys::Relation, segments: &[SegmentRef]) {
    let buf = pg_sys::ReadBufferExtended(
        index,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        0,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    fill_page(buf, &encode_meta(segments));
    pg_sys::UnlockReleaseBuffer(buf);
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
    let buf = new_buffer(index, pg_sys::ForkNumber::INIT_FORKNUM);
    fill_page(buf, &encode_meta(&[]));
    pg_sys::log_newpage_buffer(buf, true);
    pg_sys::UnlockReleaseBuffer(buf);
}

/// Copy a page's payload.
unsafe fn read_page(index: pg_sys::Relation, blk: u32, out: &mut Vec<u8>) {
    let buf = pg_sys::ReadBufferExtended(
        index,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        blk,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
    let page = pg_sys::BufferGetPage(buf) as *const u8;
    let lower = (*(page as *const pg_sys::PageHeaderData)).pd_lower as usize;
    if !(PAGE_HEADER..=BLCKSZ).contains(&lower) {
        pg_sys::UnlockReleaseBuffer(buf);
        pgrx::error!("tin: index page {blk} is corrupt");
    }
    out.extend_from_slice(std::slice::from_raw_parts(page.add(PAGE_HEADER), lower - PAGE_HEADER));
    pg_sys::UnlockReleaseBuffer(buf);
}

/// The segment table from the metapage.
pub unsafe fn read_segment_table(index: pg_sys::Relation) -> Vec<SegmentRef> {
    let mut meta = Vec::new();
    read_page(index, 0, &mut meta);
    decode_meta(&meta)
}

/// Load every segment of the index into memory.
pub unsafe fn read_index(index: pg_sys::Relation) -> Index {
    let mut segments = Vec::new();
    for r in read_segment_table(index) {
        let mut bytes = Vec::with_capacity(r.len as usize);
        for blk in r.first_block..r.first_block + r.n_blocks {
            read_page(index, blk, &mut bytes);
        }
        if bytes.len() as u64 != r.len {
            pgrx::error!(
                "tin: segment at block {} is {} bytes, expected {}",
                r.first_block,
                bytes.len(),
                r.len
            );
        }
        match Segment::from_bytes(&bytes) {
            Ok(s) => segments.push(s),
            Err(e) => pgrx::error!("tin: segment at block {} is corrupt: {e}", r.first_block),
        }
    }
    Index::from_segments(segments)
}
