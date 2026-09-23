//! How many rows `col ==> 'query'` matches, for the planner, asked of the
//! index itself.
//!
//! A flat guess (`contsel`: 0.1%) breaks the search-box query,
//! `WHERE col ==> q LIMIT 10`: expecting a match every thousand rows, the
//! planner picks a sequential scan that stops after ten, and then reads the
//! whole table because `q` matches one row. A tin index knows every term's
//! document frequency, so when the column (or expression) has one, the
//! estimate comes from its segments (see `Segment::estimate`).

use pgrx::prelude::*;
use tin_core::{Analyzer, Plan, SearchBox};

/// For queries that aren't constants (generic plans) and columns without a
/// tin index. Search-box queries are selective; assume so.
const DEFAULT_SEL: f64 = 1e-4;

/// Search-box tiers `~>` estimates count: exact, prefix, fragment.
const ESTIMATE_TIERS: u8 = 2;

/// Rows assumed within a few typos of a search-box query.
const TYPO_ROWS: f64 = 20.0;

/// `RESTRICT` estimator of `==>`.
#[pg_extern(sql = "
CREATE FUNCTION tin_restrict(internal, oid, internal, integer) RETURNS float8
    STABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
")]
fn tin_restrict(fcinfo: pg_sys::FunctionCallInfo) -> f64 {
    unsafe {
        let root = pgrx::fcinfo::pg_getarg_datum_raw(fcinfo, 0).cast_mut_ptr::<pg_sys::PlannerInfo>();
        let operator = pg_sys::Oid::from(pgrx::fcinfo::pg_getarg_datum_raw(fcinfo, 1).value() as u32);
        let args = pgrx::fcinfo::pg_getarg_datum_raw(fcinfo, 2).cast_mut_ptr::<pg_sys::List>();
        let var_relid = pgrx::fcinfo::pg_getarg_datum_raw(fcinfo, 3).value() as i32;
        let mut vardata: pg_sys::VariableStatData = std::mem::zeroed();
        let mut other: *mut pg_sys::Node = std::ptr::null_mut();
        let mut varonleft = false;
        if !pg_sys::get_restriction_variable(root, args, var_relid, &mut vardata, &mut other, &mut varonleft)
        {
            return DEFAULT_SEL;
        }
        let sel = if varonleft { estimate(&vardata, other, operator) } else { None };
        // ReleaseVariableStats
        if !vardata.statsTuple.is_null() {
            if let Some(free) = vardata.freefunc {
                free(vardata.statsTuple);
            }
        }
        sel.unwrap_or(DEFAULT_SEL)
    }
}

/// Whether `operator` is `~>` (else `==>`).
unsafe fn is_search(operator: pg_sys::Oid) -> bool {
    let name = pg_sys::get_opname(operator);
    !name.is_null() && std::ffi::CStr::from_ptr(name).to_bytes() == b"~>"
}

unsafe fn estimate(
    vardata: &pg_sys::VariableStatData,
    other: *mut pg_sys::Node,
    operator: pg_sys::Oid,
) -> Option<f64> {
    if other.is_null() || (*other).type_ != pg_sys::NodeTag::T_Const {
        return None;
    }
    let c = other as *mut pg_sys::Const;
    if (*c).constisnull {
        return Some(0.0);
    }
    // Invalid queries fail at execution, with a proper message.
    let query = String::from_datum((*c).constvalue, false)?;
    let (plan, extra_rows) = if is_search(operator) {
        // Typo tiers aren't walked (a 2-typo automaton over the dictionaries
        // cost ~10 ms per plan); they get a flat allowance instead. It
        // matters: Postgres costs one index path for both the ordered scan
        // and the bitmap scan, so only a row estimate above the LIMIT lets
        // it see that `ORDER BY col <~> q LIMIT k` stops early, while a
        // bitmap scan must evaluate every typo tier in full.
        let sb = SearchBox::parse(&query, &mut Analyzer::new());
        let typos = if sb.has_tier(3) { TYPO_ROWS } else { 0.0 };
        (sb.plan(ESTIMATE_TIERS), typos)
    } else {
        (Plan::parse(&query, &mut Analyzer::new()).ok()?, 0.0)
    };
    let rel = vardata.rel;
    if rel.is_null() {
        return None;
    }
    let am = pg_sys::get_am_oid(c"tin".as_ptr(), true);
    for info in list_ptrs::<pg_sys::IndexOptInfo>((*rel).indexlist) {
        if (*info).relam != am || (*info).ncolumns < 1 || !indexes(info, vardata.var) {
            continue;
        }
        // The planner already holds a lock on the table's indexes.
        let index = pg_sys::index_open((*info).indexoid, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        let sel = crate::scan::selectivity(index, &plan, extra_rows);
        pg_sys::index_close(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        return Some(sel);
    }
    None
}

/// Whether `info`'s (first) key is `var`: the same column, or an equal
/// expression.
unsafe fn indexes(info: *mut pg_sys::IndexOptInfo, mut var: *mut pg_sys::Node) -> bool {
    while !var.is_null() && (*var).type_ == pg_sys::NodeTag::T_RelabelType {
        var = (*(var as *mut pg_sys::RelabelType)).arg.cast();
    }
    if var.is_null() {
        return false;
    }
    let key = *(*info).indexkeys;
    if key != 0 {
        (*var).type_ == pg_sys::NodeTag::T_Var && (*(var as *mut pg_sys::Var)).varattno as i32 == key
    } else {
        list_ptrs::<pg_sys::Node>((*info).indexprs)
            .next()
            .is_some_and(|e| pg_sys::equal(e.cast(), var.cast()))
    }
}

unsafe fn list_ptrs<T>(list: *mut pg_sys::List) -> impl Iterator<Item = *mut T> {
    let n = if list.is_null() { 0 } else { (*list).length as usize };
    (0..n).map(move |i| (*(*list).elements.add(i)).ptr_value.cast())
}
