//! `ambuild`: stream the heap into memory-bounded segments (each followed by
//! its liveness bitmap, initially "every indexed tuple").
//!
//! The heap is scanned serially (`allow_sync = false`, so it starts at block
//! 0), which yields tuples in block order. Within a block, HOT chains can
//! report a root offset after higher offsets, so each block's tuples are
//! buffered and sorted before they reach a segment builder.
//!
//! **Parallel build.** Segment building (tokenizing, grams, sorting, FST and
//! postings encoding) is nearly all of a build's time, and it is pure Rust.
//! With `max_parallel_maintenance_workers` > 0, the backend only scans: it
//! cuts the heap into batches of whole blocks and hands them to that many
//! worker threads (capped by the CPU count), which build one segment per
//! batch and never call into Postgres. (Like Postgres's own parallel builds,
//! each thread, the backend's included, needs 32MB of the memory budget.) Workers run with every signal
//! blocked, so Postgres's handlers only ever run on the backend's thread;
//! while it waits for them the backend keeps checking for interrupts, and
//! the pool is joined however the build ends. With 0 workers, the backend
//! builds segments itself, closing one at a block boundary once its builder
//! outgrows `maintenance_work_mem`.
//!
//! Closed segments are held in memory while they fit in half of
//! `maintenance_work_mem`, and merged into one at the end: every dictionary
//! walk (prefixes, typos) then touches one FST instead of several, which
//! halved typo search at 5M rows. Past that, a serial build writes them out
//! as they close, so memory stays bounded however large the table is; a
//! parallel build (whose batch segments are small) merges what it holds into
//! one segment and writes that, past a quarter of the budget. Segments
//! from a parallel build come back in any order; nothing depends on it
//! (flushed segments overlap in blocks anyway).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pgrx::prelude::*;
use pgrx::PgMemoryContexts;
use tin_core::{Merge, MergedPart, Segment, SegmentBuilder, Tid};

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
    /// Worker threads (parallel build), and the batch being gathered.
    pool: Option<Pool>,
    /// Build threads (0: a serial build).
    workers: usize,
    batch: Vec<(Tid, String)>,
    batch_text: usize,
}

impl BuildState {
    unsafe fn flush_page(&mut self) {
        let Some(block) = self.block else { return };
        self.page.sort_unstable_by_key(|(off, _)| *off);
        if let Some(pool) = &self.pool {
            for (off, text) in self.page.drain(..) {
                self.batch_text += text.len();
                self.batch.push((Tid::new(block, off), text));
            }
            if pool.batch_full(self.batch.len(), self.batch_text) {
                self.dispatch();
            }
            return;
        }
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
        self.take_segment(builder.finish());
    }

    /// A closed segment: hold it, or write it out.
    unsafe fn take_segment(&mut self, seg: Segment) {
        if self.spilled {
            return self.write_segment(&seg);
        }
        self.held_bytes += seg.size_bytes();
        self.held.push(seg);
        if self.workers == 0 {
            if self.held_bytes > self.budget / 2 {
                self.spill();
            }
        } else if self.held_bytes > self.budget / 4 {
            // Batch segments are small: merge what is held into one and
            // write that, rather than many. Held, merged output and the
            // builders (half the budget) then fit in the budget.
            self.finish();
        }
    }

    /// Send the gathered batch to the workers; wait while they're all busy.
    unsafe fn dispatch(&mut self) {
        let rows = std::mem::take(&mut self.batch);
        let text = std::mem::take(&mut self.batch_text);
        if rows.is_empty() {
            return;
        }
        self.pool.as_mut().expect("parallel build").send(Batch { rows, text });
        while self.pool.as_ref().unwrap().busy() {
            self.receive();
        }
        while let Some(seg) = self.pool.as_mut().unwrap().try_take() {
            self.take_segment(seg);
        }
    }

    /// Wait for one batch's segment.
    unsafe fn receive(&mut self) {
        let seg = self.pool.as_mut().expect("parallel build").take();
        self.take_segment(seg);
    }

    /// Build what is left and wait for every batch.
    unsafe fn drain(&mut self) {
        if self.pool.is_none() {
            return;
        }
        self.dispatch();
        while self.pool.as_ref().unwrap().in_flight > 0 {
            self.receive();
        }
        let pool = self.pool.take().unwrap(); // joined when dropped, below
        debug1!("tin: built {} batches on {} threads", pool.batches, pool.threads.len());
    }

    unsafe fn spill(&mut self) {
        for seg in std::mem::take(&mut self.held) {
            self.write_segment(&seg);
        }
        self.held_bytes = 0;
        self.spilled = self.workers == 0;
    }

    /// Write what is held: merged into one segment if there are several.
    unsafe fn finish(&mut self) {
        if self.held.len() > 1 {
            let inputs: Vec<(&Segment, Option<&[u64]>)> = self.held.iter().map(|s| (s, None)).collect();
            let merged = if self.workers > 0 {
                merge_parallel(&inputs, self.workers)
            } else {
                Segment::merge(&inputs).expect("held segments have documents")
            };
            self.held.clear();
            self.held_bytes = 0;
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

/// `maintenance_work_mem` a parallel build needs per thread (the backend's
/// included), as Postgres requires for its own parallel index builds.
const MIN_PARTICIPANT_BUDGET: usize = 32 << 20;

/// Rows of whole heap blocks, in tid order, and their text size.
struct Batch {
    rows: Vec<(Tid, String)>,
    text: usize,
}

/// A built batch: its segment, and the builder's peak size per text byte.
type Built = Result<(Segment, f64), String>;

/// Worker threads building one segment per [`Batch`].
struct Pool {
    jobs: Option<Sender<Batch>>,
    results: Receiver<Built>,
    threads: Vec<JoinHandle<()>>,
    /// Set when the build is abandoned: workers drop their batch.
    cancel: Arc<AtomicBool>,
    in_flight: usize,
    batches: usize,
    /// Builder memory allowed per batch.
    batch_budget: usize,
    /// Builder bytes per text byte, the worst seen (a guess until then).
    ratio: f64,
}

impl Pool {
    /// `workers` threads; builders together get half of `budget` (the other
    /// half holds closed segments).
    unsafe fn start(workers: usize, grams: bool, budget: usize) -> Pool {
        let (jobs, job_rx) = mpsc::channel::<Batch>();
        let (done, results) = mpsc::channel::<Built>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let cancel = Arc::new(AtomicBool::new(false));
        let mask = SignalsBlocked::new();
        let threads = (0..workers)
            .map(|i| {
                let (job_rx, done, cancel) = (job_rx.clone(), done.clone(), cancel.clone());
                std::thread::Builder::new()
                    .name(format!("tin build {i}"))
                    .spawn(move || work(&job_rx, &done, grams, &cancel))
            })
            .collect::<Result<Vec<_>, _>>();
        drop(mask);
        let threads = threads.unwrap_or_else(|e| error!("tin: could not start build threads: {e}"));
        Pool {
            jobs: Some(jobs),
            results,
            batch_budget: budget / 2 / (workers + 1),
            threads,
            cancel,
            in_flight: 0,
            batches: 0,
            ratio: if grams { 32.0 } else { 8.0 },
        }
    }

    fn batch_full(&self, rows: usize, text: usize) -> bool {
        // Per row: its tid in `docs` and a width, at least.
        (text as f64 * self.ratio) as usize + rows * 16 > self.batch_budget
    }

    fn send(&mut self, batch: Batch) {
        self.jobs.as_ref().unwrap().send(batch).expect("build threads alive while jobs remain");
        self.in_flight += 1;
        self.batches += 1;
    }

    /// Every worker has a batch, and one more is queued.
    fn busy(&self) -> bool {
        self.in_flight > self.threads.len()
    }

    fn accept(&mut self, built: Built) -> Segment {
        self.in_flight -= 1;
        match built {
            Ok((seg, ratio)) => {
                self.ratio = self.ratio.max(ratio);
                seg
            }
            Err(e) => error!("tin: build thread failed: {e}"),
        }
    }

    fn try_take(&mut self) -> Option<Segment> {
        let built = self.results.try_recv().ok()?;
        Some(self.accept(built))
    }

    /// Wait for a result, handling interrupts meanwhile.
    fn take(&mut self) -> Segment {
        loop {
            match self.results.recv_timeout(Duration::from_millis(20)) {
                Ok(built) => return self.accept(built),
                Err(RecvTimeoutError::Timeout) => {
                    pgrx::check_for_interrupts!();
                }
                Err(RecvTimeoutError::Disconnected) => error!("tin: build threads exited early"),
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.jobs = None; // workers exit once the queue is empty
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Blocks every signal on this thread while it lives, so threads spawned
/// meanwhile (which inherit the mask) never run Postgres's handlers.
struct SignalsBlocked(libc::sigset_t);

impl SignalsBlocked {
    fn new() -> SignalsBlocked {
        // SAFETY: plain sigset_t manipulation of this thread's mask.
        unsafe {
            let mut all: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut all);
            libc::pthread_sigmask(libc::SIG_SETMASK, &all, &mut old);
            SignalsBlocked(old)
        }
    }
}

impl Drop for SignalsBlocked {
    fn drop(&mut self) {
        // SAFETY: restores the mask saved in `new`.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
    }
}

/// Sets a flag when dropped (also when unwinding from an error).
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// [`Segment::merge`] split by term range over `threads` threads, while the
/// backend joins the parts into the dictionary, handling interrupts. Some
/// terms have far more postings than others, so there are several parts
/// per thread.
fn merge_parallel(inputs: &[(&Segment, Option<&[u64]>)], threads: usize) -> Segment {
    let m = Merge::new(inputs).expect("held segments have documents");
    let cuts = m.split_points(threads * 4);
    let bounds: Vec<Option<&[u8]>> =
        std::iter::once(None).chain(cuts.iter().map(|c| Some(c.as_slice()))).chain([None]).collect();
    let n = bounds.len() - 1;
    let next = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let mut parts: Vec<Option<MergedPart>> = (0..n).map(|_| None).collect();
    let mut writer = m.writer();
    std::thread::scope(|s| {
        // Workers stop taking parts once the backend errors out of here.
        let _cancel = SetOnDrop(&cancel);
        let (tx, rx) = mpsc::channel::<(usize, std::thread::Result<MergedPart>)>();
        let (m, bounds, next, cancel) = (&m, &bounds, &next, &cancel);
        let mask = SignalsBlocked::new();
        for _ in 0..threads {
            let tx = tx.clone();
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n || cancel.load(Ordering::Relaxed) {
                    return;
                }
                let part = catch_unwind(AssertUnwindSafe(|| m.part(bounds[i], bounds[i + 1])));
                if tx.send((i, part)).is_err() {
                    return;
                }
            });
        }
        drop(mask);
        drop(tx);
        // Parts are taken in key order, so they mostly finish in order: the
        // backend writes each into the dictionary as soon as it can.
        let mut pushed = 0;
        while pushed < n {
            if let Some(part) = parts[pushed].take() {
                writer.push(part);
                pushed += 1;
                continue;
            }
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok((i, Ok(part))) => parts[i] = Some(part),
                Ok((_, Err(p))) => error!("tin: merge thread failed: {}", panic_message(&*p)),
                Err(RecvTimeoutError::Timeout) => {
                    pgrx::check_for_interrupts!();
                }
                Err(RecvTimeoutError::Disconnected) => error!("tin: merge threads exited early"),
            }
        }
    });
    writer.finish(m)
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "panic".to_owned())
}

/// A build thread: never calls into Postgres.
fn work(jobs: &Mutex<Receiver<Batch>>, done: &Sender<Built>, grams: bool, cancel: &AtomicBool) {
    loop {
        let Ok(batch) = jobs.lock().map_err(drop).and_then(|rx| rx.recv().map_err(drop)) else {
            return;
        };
        let built = catch_unwind(AssertUnwindSafe(|| {
            let first = batch.rows[0].0.block;
            let mut b = SegmentBuilder::open_ended(first).with_grams(grams);
            for (i, (tid, text)) in batch.rows.iter().enumerate() {
                if i % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".to_owned());
                }
                b.add(*tid, text);
            }
            let ratio = b.approx_bytes() as f64 / batch.text.max(1) as f64;
            drop(batch);
            Ok((b.finish(), ratio))
        }))
        .unwrap_or_else(|p| Err(panic_message(&*p)));
        if done.send(built).is_err() {
            return;
        }
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

    let grams = crate::options::grams(index);
    let budget = (pg_sys::maintenance_work_mem as usize) * 1024;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    // Like Postgres's own parallel builds: at least 32MB per participant.
    let workers = (pg_sys::max_parallel_maintenance_workers.max(0) as usize)
        .min(cpus)
        .min((budget / MIN_PARTICIPANT_BUDGET).saturating_sub(1));
    let mut state = BuildState {
        index,
        tmp: PgMemoryContexts::new("tin build tuple"),
        block: None,
        page: Vec::new(),
        builder: SegmentBuilder::open_ended(0).with_grams(grams),
        grams,
        segments: Vec::new(),
        held: Vec::new(),
        held_bytes: 0,
        spilled: false,
        budget,
        tuples: 0,
        pool: (workers > 0).then(|| Pool::start(workers, grams, budget)),
        workers,
        batch: Vec::new(),
        batch_text: 0,
    };

    let scan = (*(*heap).rd_tableam).index_build_range_scan.expect("table AM has index_build_range_scan");
    let started = std::time::Instant::now();
    // Guarded, so an error in the scan unwinds through here and the pool's
    // threads are joined.
    let heap_tuples = pg_sys::ffi::pg_guard_ffi_boundary(|| {
        scan(
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
        )
    });
    state.flush_page();
    state.drain();
    state.finish_segment(0);
    let built = started.elapsed();
    state.finish();
    debug1!("tin: segments built in {built:.1?}, merged and written in {:.1?}", started.elapsed() - built);

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
