//! `ambuild`: stream the heap into memory-bounded segments (each followed by
//! its liveness bitmap, initially "every indexed tuple").
//!
//! The heap is scanned serially (`allow_sync = false`, so it starts at block
//! 0), which yields tuples in block order. Within a block, HOT chains can
//! report a root offset after higher offsets, so each block's tuples are
//! buffered and sorted before they reach the segment builder. A segment is
//! closed at a block boundary once its builder outgrows
//! `maintenance_work_mem`.
//!
//! Closed segments are held in memory while they fit in half of
//! `maintenance_work_mem`, and merged into one at the end: every dictionary
//! walk (prefixes, typos) then touches one FST instead of several, which
//! halved typo search at 5M rows. Past that they are written out as they
//! close, so memory stays bounded however large the table is.

use pgrx::prelude::*;
use pgrx::PgMemoryContexts;
use tin_core::{Segment, SegmentBuilder, Tid};

use crate::storage::{self, Meta, SegmentRef, MAX_SEGMENTS};

struct BuildState {
    index: pg_sys::Relation,
    /// Short-lived context for detoasting each value.
    tmp: PgMemoryContexts,
    block: Option<u32>,
    page: Vec<(u16, String)>,
    builder: SegmentBuilder,
    grams: bool,
    segments: Vec<SegmentRef>,
    /// Closed segments not written yet (to merge at the end), and their size.
    held: Vec<Segment>,
    held_bytes: usize,
    /// Segments are written as they close (too many to merge in memory).
    spilled: bool,
    budget: usize,
    tuples: u64,
}

impl BuildState {
    unsafe fn flush_page(&mut self) {
        let Some(block) = self.block else { return };
        self.page.sort_unstable_by_key(|(off, _)| *off);
        for (off, text) in self.page.drain(..) {
            self.builder.add(Tid::new(block, off), &text);
        }
        if self.builder.approx_bytes() > self.budget {
            self.finish_segment(block + 1);
        }
    }

    unsafe fn finish_segment(&mut self, next_first_block: u32) {
        let builder = std::mem::replace(
            &mut self.builder,
            SegmentBuilder::open_ended(next_first_block).with_grams(self.grams),
        );
        if builder.doc_count() == 0 {
            return;
        }
        let seg = builder.finish();
        if self.spilled {
            return self.write_segment(&seg);
        }
        self.held_bytes += seg.size_bytes();
        self.held.push(seg);
        if self.held_bytes > self.budget / 2 {
            self.spill();
        }
    }

    unsafe fn spill(&mut self) {
        for seg in std::mem::take(&mut self.held) {
            self.write_segment(&seg);
        }
        self.spilled = true;
    }

    /// Write what is held: merged into one segment if there are several.
    unsafe fn finish(&mut self) {
        if self.held.len() > 1 {
            let inputs: Vec<(&Segment, Option<&[u64]>)> = self.held.iter().map(|s| (s, None)).collect();
            let merged = Segment::merge(&inputs).expect("held segments have documents");
            self.held.clear();
            self.write_segment(&merged);
        } else {
            self.spill();
        }
    }

    unsafe fn write_segment(&mut self, seg: &Segment) {
        if self.segments.len() == MAX_SEGMENTS {
            error!(
                "tin: index needs more than {MAX_SEGMENTS} segments; raise maintenance_work_mem and retry"
            );
        }
        let bytes = seg.to_bytes();
        let (first_block, n_blocks) = storage::write_blob(self.index, &bytes, false);
        let (live_first, live_blocks) =
            storage::write_blob(self.index, &storage::words_to_bytes(seg.docs()), false);
        self.segments.push(SegmentRef {
            id: self.segments.len() as u32,
            first_block,
            n_blocks,
            len: bytes.len() as u64,
            live_first,
            live_blocks,
        });
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn build_callback(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut std::ffi::c_void,
) {
    if *isnull {
        return; // NULL never matches
    }
    let st = &mut *(state as *mut BuildState);
    let (block, offset) = pgrx::itemptr::item_pointer_get_both(*tid);
    if st.block != Some(block) {
        st.flush_page();
        st.block = Some(block);
    }
    let datum = *values;
    let text = st.tmp.switch_to(|_| String::from_datum(datum, false)).unwrap_or_default();
    st.tmp.reset();
    st.page.push((offset, text));
    st.tuples += 1;
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambuild(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    if pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM) != 0 {
        error!("tin: index \"{}\" already contains data", name_of(index));
    }
    storage::init_metapage(index);

    let mut state = BuildState {
        index,
        tmp: PgMemoryContexts::new("tin build tuple"),
        block: None,
        page: Vec::new(),
        builder: SegmentBuilder::open_ended(0).with_grams(crate::options::grams(index)),
        grams: crate::options::grams(index),
        segments: Vec::new(),
        held: Vec::new(),
        held_bytes: 0,
        spilled: false,
        budget: (pg_sys::maintenance_work_mem as usize) * 1024,
        tuples: 0,
    };

    let scan = (*(*heap).rd_tableam).index_build_range_scan.expect("table AM has index_build_range_scan");
    let heap_tuples = scan(
        heap,
        index,
        index_info,
        false, // allow_sync: must start at block 0 so tids arrive in order
        false, // anyvisible
        true,  // progress
        0,
        u32::MAX, // InvalidBlockNumber: to the end
        Some(build_callback),
        &mut state as *mut BuildState as *mut std::ffi::c_void,
        std::ptr::null_mut(),
    );
    state.flush_page();
    state.finish_segment(0);
    state.finish();

    let mut meta = Meta::empty();
    meta.next_segment_id = state.segments.len() as u32;
    meta.segments = std::mem::take(&mut state.segments);
    storage::write_meta_unlogged(index, &meta);
    storage::log_all_pages(index);

    let mut result = PgBox::<pg_sys::IndexBuildResult>::alloc0();
    result.heap_tuples = heap_tuples;
    result.index_tuples = state.tuples as f64;
    result.into_pg()
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambuildempty(index: pg_sys::Relation) {
    storage::write_empty_init_fork(index);
}

pub unsafe fn name_of(rel: pg_sys::Relation) -> String {
    std::ffi::CStr::from_ptr((*(*rel).rd_rel).relname.data.as_ptr()).to_string_lossy().into_owned()
}
