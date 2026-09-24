//! Planner support functions giving `tin_match` (`==>`) and `tin_score` a
//! per-row cost that grows with the text they read.
//!
//! Both re-read the row: split it into words, then match or score. That is
//! ~0.4 µs for a 36-byte identifier but ~5 µs for a 600-byte post, so one
//! fixed `COST` is wrong for one of them. Declared too cheap, the planner
//! sees nothing to gain from parallel workers (it divides only CPU cost
//! among them) and undercharges sequential scans that recheck every row.
//!
//! Measured on the benchmark server (one cost unit ≈ 3.3 µs there): a
//! recheck costs about `10 + average width` `cpu_operator_cost`s, the width
//! coming from the column's statistics (`pg_stats.avg_width`). Scoring also
//! hashes every word, so it counts the width twice. Without statistics, or
//! when the text isn't a plain column, a default width is assumed.
//!
//! The same cost goes into the index's own estimate ([`recheck_cost`]):
//! Postgres charges a plain index scan nothing for rechecking its index
//! condition (the access method must account for it), so a phrase of
//! common words, which rechecks most of the table, looked cheaper as an
//! index scan than as a parallel sequential scan that is 3× faster.

use pgrx::prelude::*;
use pgrx::{Internal, PgList};

/// Width assumed without statistics (Postgres's own guess for text).
const DEFAULT_WIDTH: f64 = 32.0;

/// Fixed part of a call, in `cpu_operator_cost`s.
const BASE: f64 = 10.0;

/// Answers a `SupportRequestCost` for a call whose text is argument
/// `doc_arg`, costing `per_byte` operator costs per byte of it.
unsafe fn cost_support(req: Internal, doc_arg: usize, per_byte: f64) -> Internal {
    // "Not handled" is a null pointer (not SQL NULL: the planner calls
    // support functions with FunctionCall1, which rejects NULL results).
    let unhandled = Internal::from(Some(pg_sys::Datum::from(0usize)));
    let Some(datum) = req.unwrap() else { return unhandled };
    let node = datum.cast_mut_ptr::<pg_sys::Node>();
    if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestCost {
        return unhandled;
    }
    let r = node as *mut pg_sys::SupportRequestCost;
    let width = doc_width((*r).root, (*r).node, doc_arg).unwrap_or(DEFAULT_WIDTH);
    (*r).startup = 0.0;
    (*r).per_tuple = (BASE + per_byte * width) * pg_sys::cpu_operator_cost;
    Internal::from(Some(datum))
}

/// Average stored width of argument `n` of the call `expr`, if it is a
/// column of a table with statistics.
unsafe fn doc_width(root: *mut pg_sys::PlannerInfo, expr: *mut pg_sys::Node, n: usize) -> Option<f64> {
    if root.is_null() || expr.is_null() {
        return None;
    }
    let args = match (*expr).type_ {
        pg_sys::NodeTag::T_OpExpr => (*(expr as *mut pg_sys::OpExpr)).args,
        pg_sys::NodeTag::T_FuncExpr => (*(expr as *mut pg_sys::FuncExpr)).args,
        _ => return None,
    };
    let arg = PgList::<pg_sys::Node>::from_pg(args).get_ptr(n)?;
    if (*arg).type_ != pg_sys::NodeTag::T_Var {
        return None;
    }
    let var = arg as *mut pg_sys::Var;
    let varno = (*var).varno;
    if varno <= 0 || varno >= (*root).simple_rel_array_size || (*var).varlevelsup != 0 {
        return None;
    }
    let rte = *(*root).simple_rte_array.add(varno as usize);
    if rte.is_null() || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
        return None;
    }
    let w = pg_sys::get_attavgwidth((*rte).relid, (*var).varattno);
    (w > 0).then_some(w as f64)
}

/// Per-row cost of `tin_match` on a column of average width `width`.
fn match_cost(width: f64) -> f64 {
    (BASE + width) * unsafe { pg_sys::cpu_operator_cost }
}

/// The cost of rechecking an index path's candidates: nothing unless a
/// `==>` condition needs a recheck (phrases, proximity, fragments). Phrase
/// and proximity candidates are estimated at twice the matches (the
/// estimator's pass rate for them is one half). Ordered scans (search box)
/// are left alone.
pub unsafe fn recheck_cost(root: *mut pg_sys::PlannerInfo, path: *mut pg_sys::IndexPath, rows: f64) -> f64 {
    if !(*path).indexorderbys.is_null() {
        return 0.0;
    }
    let info = (*path).indexinfo;
    let mut factor = 0.0f64;
    for ic in PgList::<pg_sys::IndexClause>::from_pg((*path).indexclauses).iter_ptr() {
        let clause = (*(*ic).rinfo).clause as *mut pg_sys::Node;
        if clause.is_null() || (*clause).type_ != pg_sys::NodeTag::T_OpExpr {
            continue;
        }
        let op = clause as *mut pg_sys::OpExpr;
        if crate::selectivity::is_search((*op).opno) {
            continue;
        }
        let Some(query) = PgList::<pg_sys::Node>::from_pg((*op).args).get_ptr(1) else { continue };
        if (*query).type_ != pg_sys::NodeTag::T_Const || (*(query as *mut pg_sys::Const)).constisnull {
            continue;
        }
        let Some(text) = String::from_datum((*(query as *mut pg_sys::Const)).constvalue, false) else {
            continue;
        };
        let Ok(plan) = tin_core::Plan::parse(&text, &mut tin_core::Analyzer::new()) else { continue };
        if plan.needs_recheck() {
            let positional = matches!(plan, tin_core::Plan::Recheck(_));
            factor = factor.max(if positional { 2.0 } else { 1.0 });
        }
    }
    if factor == 0.0 {
        return 0.0;
    }
    // The heap column the index is on (an expression index: default width).
    let key = *(*info).indexkeys;
    let rel = (*info).rel;
    let width = if key > 0
        && !rel.is_null()
        && (*rel).relid > 0
        && (*rel).relid < (*root).simple_rel_array_size as u32
    {
        let rte = *(*root).simple_rte_array.add((*rel).relid as usize);
        let w =
            if rte.is_null() { 0 } else { pg_sys::get_attavgwidth((*rte).relid, key as pg_sys::AttrNumber) };
        if w > 0 {
            w as f64
        } else {
            DEFAULT_WIDTH
        }
    } else {
        DEFAULT_WIDTH
    };
    factor * rows * match_cost(width)
}

/// Cost of `tin_match(doc, query)` (the `==>` operator).
#[pg_extern(immutable, parallel_safe, strict)]
fn tin_match_support(req: Internal) -> Internal {
    unsafe { cost_support(req, 0, 1.0) }
}

/// Cost of `tin_score(index, doc, query)`.
#[pg_extern(immutable, parallel_safe, strict)]
fn tin_score_support(req: Internal) -> Internal {
    unsafe { cost_support(req, 1, 2.0) }
}
