//! `pg_tin` — the tin index as a Postgres index access method.
//!
//! ```sql
//! CREATE EXTENSION pg_tin;
//! CREATE INDEX posts_body_tin ON posts USING tin (body);
//! SELECT count(*) FROM posts WHERE body ==> 'grub (uefi OR bios) -windows';
//! ```
//!
//! Phase 1 scope: build + bitmap scans. The index is read-only after
//! `CREATE INDEX`; inserts and non-HOT updates into an indexed table error
//! until Phase 2 (mutable segments + liveness bitmaps).

use pgrx::prelude::*;
use pgrx::PgRelation;
use tin_core::Analyzer;

mod build;
mod scan;
mod storage;

::pgrx::pg_module_magic!();

/// `doc ==> query`: the non-index path (sequential scans, rechecks). Uses the
/// same analyzer and query semantics as the index.
#[pg_extern(immutable, parallel_safe, strict)]
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

/// Inspect a tin index: one row per segment.
/// `SELECT * FROM tin_segments('posts_body_tin'::regclass);`
#[pg_extern(strict)]
fn tin_segments(
    index: pg_sys::Oid,
) -> TableIterator<
    'static,
    (name!(segment, i32), name!(first_block, i64), name!(blocks, i64), name!(bytes, i64)),
> {
    let rel = unsafe { PgRelation::with_lock(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    let is_tin = unsafe {
        let form = &*(*rel.as_ptr()).rd_rel;
        form.relkind == pg_sys::RELKIND_INDEX as std::ffi::c_char
            && form.relam == pg_sys::get_am_oid(c"tin".as_ptr(), false)
    };
    if !is_tin {
        error!("tin: \"{}\" is not a tin index", rel.name());
    }
    let table = unsafe { storage::read_segment_table(rel.as_ptr()) };
    TableIterator::new(
        table
            .into_iter()
            .enumerate()
            .map(|(i, r)| (i as i32, r.first_block as i64, r.n_blocks as i64, r.len as i64)),
    )
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
    am.aminsert = Some(aminsert);
    am.ambulkdelete = Some(ambulkdelete);
    am.amvacuumcleanup = Some(amvacuumcleanup);
    am.amcostestimate = Some(amcostestimate);
    am.amoptions = Some(amoptions);
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
    RESTRICT = contsel,
    JOIN = contjoinsel
);
COMMENT ON OPERATOR ==> (text, text) IS 'full-text match: document ==> tin query';

CREATE OPERATOR CLASS text_tin_ops DEFAULT FOR TYPE text USING tin AS
    OPERATOR 1 ==> (text, text);
"#,
    name = "tin_operator",
    requires = [tin_match, tin_handler]
);

#[pg_guard]
#[allow(clippy::too_many_arguments)]
unsafe extern "C-unwind" fn aminsert(
    index: pg_sys::Relation,
    _values: *mut pg_sys::Datum,
    _isnull: *mut bool,
    _heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    error!(
        "tin: index \"{}\" is read-only in this version; load the data first, then CREATE INDEX (or REINDEX)",
        build::name_of(index)
    );
}

/// Phase 1 keeps dead tids (see `scan.rs` for why that is safe); Phase 2
/// clears them in per-segment liveness bitmaps.
#[pg_guard]
unsafe extern "C-unwind" fn ambulkdelete(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _callback_state: *mut std::ffi::c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    if stats.is_null() {
        PgBox::<pg_sys::IndexBulkDeleteResult>::alloc0().into_pg()
    } else {
        stats
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn amvacuumcleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let stats =
        if stats.is_null() { PgBox::<pg_sys::IndexBulkDeleteResult>::alloc0().into_pg() } else { stats };
    (*stats).num_pages =
        pg_sys::RelationGetNumberOfBlocksInFork((*info).index, pg_sys::ForkNumber::MAIN_FORKNUM);
    stats
}

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
unsafe extern "C-unwind" fn amoptions(_reloptions: pg_sys::Datum, _validate: bool) -> *mut pg_sys::bytea {
    std::ptr::null_mut()
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
