//! `pg_tin` — the tin index as a Postgres index access method.
//!
//! ```sql
//! CREATE EXTENSION pg_tin;
//! CREATE INDEX posts_body_tin ON posts USING tin (body);
//! SELECT count(*) FROM posts WHERE body ==> 'grub (uefi OR bios) -windows';
//! ```
//!
//! Writes go to a pending list that is flushed into immutable segments;
//! VACUUM clears deleted tuples from per-segment liveness bitmaps.

use pgrx::prelude::*;
use pgrx::{GucContext, GucFlags, GucRegistry, PgRelation};
use tin_core::Analyzer;

mod build;
mod options;
mod pending;
mod scan;
mod selectivity;
mod storage;
mod write;

::pgrx::pg_module_magic!();

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    options::register();
    GucRegistry::define_int_guc(
        c"tin.pending_list_limit",
        c"Size of a tin index's pending list before it is flushed into a new segment.",
        c"Inserted tuples collect in a pending list that queries scan directly; past this size \
          the list is turned into an immutable segment. Also flushed by VACUUM and tin_flush().",
        &write::PENDING_LIST_LIMIT,
        64,
        i32::MAX / 1024,
        GucContext::Userset,
        GucFlags::UNIT_KB,
    );
}

/// `doc ==> query`: the non-index path (sequential scans, rechecks). Uses the
/// same analyzer and query semantics as the index. Analyzing each document
/// costs roughly ten simple operators (0.4 µs a row on short identifiers),
/// hence `cost = 10`.
#[pg_extern(immutable, parallel_safe, strict, cost = 10)]
fn tin_match(doc: &str, query: &str) -> bool {
    thread_local! {
        static LAST: std::cell::RefCell<Option<(String, tin_core::Plan)>> =
            const { std::cell::RefCell::new(None) };
    }
    LAST.with(|last| {
        let mut last = last.borrow_mut();
        if last.as_ref().is_none_or(|(q, _)| q != query) {
            *last = Some((query.to_owned(), scan::parse_query(query)));
        }
        last.as_ref().unwrap().1.matches_text(doc, &mut Analyzer::new())
    })
}

/// Open `index` (an OID; pass `'name'::regclass`) and check it is a tin index.
fn open_tin(index: pg_sys::Oid, lockmode: u32) -> PgRelation {
    let rel = unsafe { PgRelation::with_lock(index, lockmode as pg_sys::LOCKMODE) };
    let is_tin = unsafe {
        let form = &*(*rel.as_ptr()).rd_rel;
        form.relkind == pg_sys::RELKIND_INDEX as std::ffi::c_char
            && form.relam == pg_sys::get_am_oid(c"tin".as_ptr(), false)
    };
    if !is_tin {
        error!("tin: \"{}\" is not a tin index", rel.name());
    }
    rel
}

/// One row per segment: `SELECT * FROM tin_segments('idx'::regclass)`.
#[pg_extern(strict)]
fn tin_segments(
    index: pg_sys::Oid,
) -> TableIterator<
    'static,
    (
        name!(segment, i32),
        name!(first_block, i64),
        name!(blocks, i64),
        name!(bytes, i64),
        name!(live_tuples, i64),
    ),
> {
    let rel = open_tin(index, pg_sys::AccessShareLock);
    let (meta, live) = unsafe { scan::stats(rel.as_ptr()) };
    TableIterator::new(
        meta.segments
            .into_iter()
            .zip(live)
            .map(|(r, l)| (r.id as i32, r.first_block as i64, r.n_blocks as i64, r.len as i64, l as i64)),
    )
}

/// Pending-list size and totals: `SELECT * FROM tin_stats('idx'::regclass)`.
#[pg_extern(strict)]
fn tin_stats(
    index: pg_sys::Oid,
) -> TableIterator<
    'static,
    (
        name!(segments, i32),
        name!(live_tuples, i64),
        name!(pending_tuples, i64),
        name!(pending_bytes, i64),
        name!(generation, i64),
    ),
> {
    let rel = open_tin(index, pg_sys::AccessShareLock);
    let (meta, live) = unsafe { scan::stats(rel.as_ptr()) };
    TableIterator::once((
        meta.segments.len() as i32,
        live.iter().sum::<u64>() as i64,
        meta.pending_count as i64,
        meta.pending_bytes as i64,
        meta.generation as i64,
    ))
}

/// Flush the pending list into a new segment now; returns tuples flushed.
#[pg_extern(strict)]
fn tin_flush(index: pg_sys::Oid) -> i64 {
    let rel = open_tin(index, pg_sys::RowExclusiveLock);
    unsafe { write::flush(rel.as_ptr()) as i64 }
}

/// The index access method handler.
#[pg_extern(sql = "
CREATE FUNCTION tin_handler(internal) RETURNS index_am_handler
    PARALLEL SAFE IMMUTABLE STRICT LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
CREATE ACCESS METHOD tin TYPE INDEX HANDLER tin_handler;
COMMENT ON ACCESS METHOD tin IS 'tin: ctid two-level-bitmap full-text index';
")]
fn tin_handler(_fcinfo: pg_sys::FunctionCallInfo) -> PgBox<pg_sys::IndexAmRoutine> {
    let mut am = unsafe { PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine) };
    // Everything not set here stays zero/false/None (alloc_node zeroes).
    am.amstrategies = 1;
    am.amsupport = 0;
    am.amoptionalkey = false; // a scan always needs a ==> condition
    am.amusemaintenanceworkmem = true;
    am.amkeytype = pg_sys::InvalidOid;

    am.ambuild = Some(build::ambuild);
    am.ambuildempty = Some(build::ambuildempty);
    am.aminsert = Some(write::aminsert);
    am.ambulkdelete = Some(write::ambulkdelete);
    am.amvacuumcleanup = Some(write::amvacuumcleanup);
    am.amcostestimate = Some(amcostestimate);
    am.amoptions = Some(options::amoptions);
    am.amvalidate = Some(amvalidate);
    am.ambeginscan = Some(scan::ambeginscan);
    am.amrescan = Some(scan::amrescan);
    am.amgetbitmap = Some(scan::amgetbitmap);
    am.amendscan = Some(scan::amendscan);
    // No amgettuple: tin only serves bitmap scans (like GIN).
    am.into_pg_boxed()
}

extension_sql!(
    r#"
CREATE OPERATOR ==> (
    LEFTARG = text,
    RIGHTARG = text,
    FUNCTION = tin_match,
    RESTRICT = tin_restrict,
    JOIN = contjoinsel
);
COMMENT ON OPERATOR ==> (text, text) IS 'full-text match: document ==> tin query';

CREATE OPERATOR CLASS text_tin_ops DEFAULT FOR TYPE text USING tin AS
    OPERATOR 1 ==> (text, text);
"#,
    name = "tin_operator",
    requires = [tin_match, tin_handler, tin_restrict]
);

#[pg_guard]
#[allow(clippy::too_many_arguments)]
unsafe extern "C-unwind" fn amcostestimate(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    let mut costs = pg_sys::GenericCosts::default();
    pg_sys::genericcostestimate(root, path, loop_count, &mut costs);
    *startup = costs.indexStartupCost;
    *total = costs.indexTotalCost;
    *selectivity = costs.indexSelectivity;
    *correlation = costs.indexCorrelation;
    *pages = costs.numIndexPages;
}

#[pg_guard]
unsafe extern "C-unwind" fn amvalidate(_opclass: pg_sys::Oid) -> bool {
    true
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
