//! Index options: `CREATE INDEX … USING tin (col) WITH (grams = true)`.
//!
//! `grams` also indexes every term's character trigrams, so `*fragment*`
//! queries become an AND over trigrams (plus a heap recheck) instead of a
//! scan of the whole dictionary. Worth it for identifier search; for long
//! text it roughly triples the index.

use std::sync::atomic::{AtomicU32, Ordering};

use pgrx::pg_sys;
use pgrx::prelude::*;

static KIND: AtomicU32 = AtomicU32::new(0);

#[repr(C)]
struct TinOptions {
    vl_len_: i32,
    grams: bool,
}

/// Register the option (from `_PG_init`).
pub fn register() {
    unsafe {
        let kind = pg_sys::add_reloption_kind();
        KIND.store(kind, Ordering::Relaxed);
        pg_sys::add_bool_reloption(
            kind,
            c"grams".as_ptr(),
            c"Also index character trigrams, for fast *fragment* search".as_ptr(),
            false,
            pg_sys::AccessExclusiveLock as pg_sys::LOCKMODE,
        );
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amoptions(reloptions: pg_sys::Datum, validate: bool) -> *mut pg_sys::bytea {
    let elems = [pg_sys::relopt_parse_elt {
        optname: c"grams".as_ptr(),
        opttype: pg_sys::relopt_type::RELOPT_TYPE_BOOL,
        offset: std::mem::offset_of!(TinOptions, grams) as i32,
        isset_offset: 0,
    }];
    pg_sys::build_reloptions(
        reloptions,
        validate,
        KIND.load(Ordering::Relaxed),
        std::mem::size_of::<TinOptions>(),
        elems.as_ptr(),
        elems.len() as i32,
    ) as *mut pg_sys::bytea
}

/// Whether `index` was created `WITH (grams = true)`.
pub unsafe fn grams(index: pg_sys::Relation) -> bool {
    let opts = (*index).rd_options as *const TinOptions;
    !opts.is_null() && (*opts).grams
}
