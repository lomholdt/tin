//! Ranked retrieval for a [`SearchBox`]: matches come out tier by tier
//! (exact, prefix, fragment, typo), each tuple once, so a caller that wants
//! the best k stops as soon as it has them.
//!
//! [`Ranked`] is pull-based and keeps no borrows between calls: every
//! [`next`](Ranked::next) is handed the [`Sources`] (segments with liveness,
//! plus not-yet-indexed records), which must be the same, in the same order,
//! for the life of a scan.
//!
//! * A single-term query without a filter streams its prefix and typo tiers
//!   from the term dictionary a few terms at a time. `MSKU6` matches ~100k
//!   numbers, and the first 10 are all a `LIMIT 10` needs.
//! * Everything else runs the tier's plan over each segment at once.
//! * With a `filter` (other index conditions), every tier is ANDed with it,
//!   and filter matches that match no tier come last ([`NO_TIER`]).

use std::collections::VecDeque;

use rustc_hash::FxHashSet;

use crate::query::{Plan, SortedTerms};
use crate::search::{Matcher, SearchBox, MAX_TIER};
use crate::segment::{Segment, TermFilter};
use crate::tid::Tid;

/// The tier of filter matches that match no tier of the query.
pub const NO_TIER: u8 = MAX_TIER + 1;

/// Terms taken from the dictionary per streaming step.
const STREAM_TERMS: usize = 32;

/// A document not in any segment yet (a pending-list record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDoc {
    pub tid: Tid,
    /// Sorted, distinct analyzed terms.
    pub terms: Vec<String>,
}

/// What a scan reads: segments in a fixed order with their liveness bitmaps,
/// and pending documents.
pub struct Sources<'a> {
    pub segments: Vec<(&'a Segment, Option<&'a [u64]>)>,
    pub pending: &'a [PendingDoc],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub tid: Tid,
    /// Its tier, or a lower bound when `tier_is_bound`.
    pub tier: u8,
    /// A candidate: the conditions must be checked against the tuple.
    pub recheck: bool,
    /// `tier` is only a lower bound (fragment candidates): recompute it.
    pub tier_is_bound: bool,
}

pub struct Ranked {
    query: SearchBox,
    matcher: Matcher,
    filter: Option<Plan>,
    /// Per pending document, computed once per scan: its tier (`NO_TIER`
    /// if none) and whether it passes the filter.
    pending: Option<Vec<(u8, bool)>>,
    /// Current tier; `NO_TIER` for the filter-only rest; past it, done.
    tier: u8,
    step: Step,
    emitted: FxHashSet<Tid>,
    buf: VecDeque<Hit>,
}

enum Step {
    /// The current tier hasn't started.
    Start,
    /// Streaming the current tier: segment `seg`, after term `after`.
    Stream { seg: usize, after: Option<Vec<u8>> },
    /// The current tier is fully buffered.
    Done,
}

impl Ranked {
    pub fn new(query: SearchBox, filter: Option<Plan>) -> Ranked {
        Ranked {
            matcher: query.matcher(),
            query,
            filter,
            pending: None,
            tier: 0,
            step: Step::Start,
            emitted: FxHashSet::default(),
            buf: VecDeque::new(),
        }
    }

    pub fn next(&mut self, src: &Sources<'_>) -> Option<Hit> {
        loop {
            if let Some(h) = self.buf.pop_front() {
                return Some(h);
            }
            if self.tier > NO_TIER {
                return None;
            }
            match std::mem::replace(&mut self.step, Step::Done) {
                Step::Done => {
                    self.tier += 1;
                    self.step = Step::Start;
                }
                Step::Start => self.start_tier(src),
                Step::Stream { seg, after } => self.stream(src, seg, after),
            }
        }
    }

    /// Whether the current tier streams from the dictionary.
    fn streams(&self) -> bool {
        self.query.terms().len() == 1 && self.filter.is_none() && matches!(self.tier, 1 | 3 | 4)
    }

    fn start_tier(&mut self, src: &Sources<'_>) {
        if self.tier == NO_TIER {
            if let Some(f) = self.filter.clone() {
                let recheck = f.needs_recheck();
                self.collect(src, &f, NO_TIER, recheck, false);
            }
            return;
        }
        if !self.query.has_tier(self.tier) {
            return;
        }
        if self.streams() {
            self.step = Step::Stream { seg: 0, after: None };
            return;
        }
        // Rows of lower tiers are already out; a single term needs only
        // this tier's own alternative.
        let tier_plan = match self.query.terms() {
            [t] if self.filter.is_none() => match self.tier {
                0 => Plan::Term(t.clone()),
                _ => Plan::Fragment(t.clone()), // tier 2; 1, 3, 4 stream
            },
            _ => self.query.plan(self.tier),
        };
        let bound = tier_plan.needs_recheck();
        let plan = match &self.filter {
            Some(f) => Plan::And(vec![tier_plan, f.clone()]),
            None => tier_plan,
        };
        let recheck = plan.needs_recheck();
        self.collect(src, &plan, self.tier, recheck, bound);
    }

    /// Buffer every unseen match of `plan`, as tier `tier`.
    fn collect(&mut self, src: &Sources<'_>, plan: &Plan, tier: u8, recheck: bool, bound: bool) {
        let mut tids = Vec::new();
        for (seg, live) in &src.segments {
            tids.clear();
            seg.collect_live(plan, *live, &mut tids);
            for &tid in &tids {
                if self.emitted.insert(tid) {
                    self.buf.push_back(Hit { tid, tier, recheck, tier_is_bound: bound });
                }
            }
        }
        // Pending documents are matched exactly.
        self.pending_hits(src, |t, ok| ok && t <= tier, tier);
    }

    /// Buffer pending documents for which `pick(tier, passes_filter)`, as
    /// tier `tier`.
    fn pending_hits(&mut self, src: &Sources<'_>, pick: impl Fn(u8, bool) -> bool, tier: u8) {
        // A filter with phrases or proximity only narrows pending documents
        // down to candidates, like segments.
        let recheck = self.filter.as_ref().is_some_and(Plan::needs_recheck);
        let (matcher, filter) = (&self.matcher, &self.filter);
        let info = self.pending.get_or_insert_with(|| {
            src.pending
                .iter()
                .map(|d| {
                    let t = matcher.tier_of_terms(&d.terms).unwrap_or(NO_TIER);
                    (t, filter.as_ref().is_none_or(|f| f.matches(&SortedTerms(&d.terms))))
                })
                .collect()
        });
        for (d, &(t, ok)) in src.pending.iter().zip(info.iter()) {
            if pick(t, ok) && self.emitted.insert(d.tid) {
                self.buf.push_back(Hit { tid: d.tid, tier, recheck, tier_is_bound: false });
            }
        }
    }

    fn stream(&mut self, src: &Sources<'_>, seg: usize, after: Option<Vec<u8>>) {
        let tier = self.tier;
        if let Some((segment, live)) = src.segments.get(seg) {
            let term = self.query.terms()[0].clone();
            let filter = match tier {
                1 => TermFilter::Prefix(&term),
                3 => TermFilter::Fuzzy(&term, 1),
                _ => TermFilter::Fuzzy(&term, 2),
            };
            let (emitted, buf) = (&mut self.emitted, &mut self.buf);
            let rest = segment.stream_terms(&filter, after.as_deref(), STREAM_TERMS, |tp| {
                segment.for_each_posting(&tp, *live, |tid| {
                    if emitted.insert(tid) {
                        buf.push_back(Hit { tid, tier, recheck: false, tier_is_bound: false });
                    }
                })
            });
            self.step = match rest {
                Some(after) => Step::Stream { seg, after: Some(after) },
                None => Step::Stream { seg: seg + 1, after: None },
            };
            return;
        }
        // Segments done: pending documents of this tier.
        self.pending_hits(src, |t, _| t <= tier, tier);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentBuilder;
    use crate::tokenize::Analyzer;

    #[test]
    fn fragment_candidates_are_flagged() {
        // Tuple 1 has the grams of `12345` split over two terms: a candidate
        // only. Tuple 2 really contains it.
        let mut b = SegmentBuilder::new(0, 1).with_grams(true);
        b.add(Tid::new(0, 1), "a1234b c2345d");
        b.add(Tid::new(0, 2), "x123456");
        let seg = b.finish();
        let src = Sources { segments: vec![(&seg, None)], pending: &[] };
        let mut r = Ranked::new(SearchBox::parse("12345", &mut Analyzer::new()), None);
        let mut hits = Vec::new();
        while let Some(h) = r.next(&src) {
            hits.push(h);
        }
        let h1 = hits.iter().find(|h| h.tid == Tid::new(0, 1)).expect("candidate returned");
        assert!(h1.recheck && h1.tier_is_bound && h1.tier == 2);
        assert!(hits.iter().any(|h| h.tid == Tid::new(0, 2) && h.tier == 2));
    }
}
