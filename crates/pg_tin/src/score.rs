//! `tin_score` / `tin_score_inspect`: BM25 (see `tin_core::score`) with the
//! collection statistics read from the index.
//!
//! Statistics are taken once per statement, index and query: row count
//! (live tuples plus pending records), average distinct terms per row (word
//! postings over indexed rows), and each term's document frequency, looked
//! up lazily in every segment's dictionary and the pending list. Deleted
//! tuples still count toward frequencies until VACUUM compacts them away,
//! as in most search engines.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pgrx::prelude::*;
use tin_core::score::{CollectionStats, Explanation, Scorer};
use tin_core::span::DocWords;
use tin_core::{Analyzer, Query};

use crate::scan::{self, IndexState};

struct Stats {
    state: Rc<RefCell<IndexState>>,
    docs: f64,
    avg_len: f64,
    df: RefCell<HashMap<String, u64>>,
}

impl CollectionStats for Stats {
    fn docs(&self) -> f64 {
        self.docs
    }

    fn avg_len(&self) -> f64 {
        self.avg_len
    }

    fn doc_freq(&self, term: &str) -> u64 {
        if let Some(&n) = self.df.borrow().get(term) {
            return n;
        }
        let st = self.state.borrow();
        let mut n: u64 = st.segments.iter().filter_map(|(_, s, _)| s.term(term)).map(|t| t.doc_count()).sum();
        n += st.pending.iter().filter(|r| r.terms.binary_search_by(|t| t.as_str().cmp(term)).is_ok()).count()
            as u64;
        self.df.borrow_mut().insert(term.to_owned(), n);
        n
    }
}

unsafe fn stats(index: pg_sys::Relation) -> Stats {
    let state = scan::state(index);
    let (docs, avg_len) = {
        let st = state.borrow();
        let (mut live, mut indexed, mut postings) = (0u64, 0u64, 0u64);
        for (_, seg, liveness) in &st.segments {
            live += liveness.iter().map(|w| w.count_ones() as u64).sum::<u64>();
            indexed += seg.meta().doc_count;
            postings += seg.word_postings();
        }
        live += st.pending.len() as u64;
        indexed += st.pending.len() as u64;
        postings += st.pending.iter().map(|r| r.terms.len() as u64).sum::<u64>();
        (live as f64, postings as f64 / indexed.max(1) as f64)
    };
    Stats { state, docs, avg_len, df: RefCell::new(HashMap::new()) }
}

/// Per statement: the index's statistics and the parsed query.
struct Cached {
    key: (pg_sys::Oid, String, pg_sys::TimestampTz, pg_sys::CommandId),
    query: Query,
    scorer: Scorer,
    stats: Stats,
}

thread_local! {
    static CACHE: RefCell<Option<Cached>> = const { RefCell::new(None) };
}

fn with_scorer<R>(index: pg_sys::Oid, query: &str, f: impl FnOnce(&Cached) -> R) -> R {
    let key = unsafe {
        (
            index,
            query.to_owned(),
            pg_sys::GetCurrentStatementStartTimestamp(),
            pg_sys::GetCurrentCommandId(false),
        )
    };
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|x| x.key != key) {
            let rel = crate::open_tin(index, pg_sys::AccessShareLock);
            let query = scan::parse_tinql(query);
            *c = Some(Cached {
                scorer: Scorer::new(&query),
                query,
                stats: unsafe { stats(rel.as_ptr()) },
                key,
            });
        }
        f(c.as_ref().unwrap())
    })
}

pub fn explain(index: pg_sys::Oid, doc: &str, query: &str) -> Explanation {
    with_scorer(index, query, |c| c.scorer.explain(&DocWords::new(doc, &mut Analyzer::new()), &c.stats))
}

pub fn score(index: pg_sys::Oid, doc: &str, query: &str) -> f64 {
    with_scorer(index, query, |c| c.scorer.score(&DocWords::new(doc, &mut Analyzer::new()), &c.stats))
}

/// Whether `doc` matches (for the inspection output).
pub fn matches(index: pg_sys::Oid, doc: &str, query: &str) -> bool {
    with_scorer(index, query, |c| c.query.matches_text(doc, &mut Analyzer::new()))
}
