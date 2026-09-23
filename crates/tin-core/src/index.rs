//! An index = an ordered list of segments over disjoint, ascending block
//! ranges.
//!
//! The initial build mirrors TIN's: split the heap into `n` block ranges and
//! build one immutable segment per range in parallel. Because segments never
//! overlap, query results are the concatenation of per-segment results, still
//! in heap (tid) order. (Mutable segments with overlapping ranges arrive in
//! Phase 2.)

use std::collections::BTreeMap;

use rayon::prelude::*;

use crate::postings::Encoding;
use crate::query::Plan;
use crate::segment::{ClassStats, Segment, SegmentBuilder};
use crate::tid::Tid;

pub struct Index {
    segments: Vec<Segment>,
}

/// Build-time size accounting, merged across segments.
pub type SizeStats<C> = BTreeMap<(C, Encoding), ClassStats>;

impl Index {
    pub fn from_segments(segments: Vec<Segment>) -> Self {
        for w in segments.windows(2) {
            assert!(
                w[0].meta().end_block <= w[1].meta().first_block,
                "segments must cover disjoint ascending block ranges"
            );
        }
        Index { segments }
    }

    /// Build from `docs`, which must be sorted by tid (strictly ascending),
    /// using `n_segments` block ranges built in parallel.
    pub fn build<S: AsRef<str> + Sync>(docs: &[(Tid, S)], n_segments: usize) -> Self {
        Self::build_with_stats(docs, n_segments, |_, _| ()).0
    }

    pub fn build_with_stats<S, C, F>(
        docs: &[(Tid, S)],
        n_segments: usize,
        class_of: F,
    ) -> (Self, SizeStats<C>)
    where
        S: AsRef<str> + Sync,
        C: Ord + Copy + Send,
        F: Fn(u64, u64) -> C + Sync,
    {
        assert!(n_segments > 0);
        let end_block = docs.last().map_or(0, |(t, _)| t.block as u64 + 1);
        let bounds: Vec<u32> =
            (0..=n_segments).map(|i| (end_block * i as u64 / n_segments as u64) as u32).collect();

        let parts: Vec<(Segment, SizeStats<C>)> = (0..n_segments)
            .into_par_iter()
            .filter(|&i| bounds[i] < bounds[i + 1])
            .map(|i| {
                let (lo, hi) = (bounds[i], bounds[i + 1]);
                let start = docs.partition_point(|(t, _)| t.block < lo);
                let end = docs.partition_point(|(t, _)| t.block < hi);
                let mut b = SegmentBuilder::new(lo, hi);
                for (tid, text) in &docs[start..end] {
                    b.add(*tid, text.as_ref());
                }
                b.finish_with_stats(&class_of)
            })
            .collect();

        let mut stats = SizeStats::new();
        let mut segments = Vec::with_capacity(parts.len());
        for (seg, s) in parts {
            for (k, v) in s {
                *stats.entry(k).or_default() += v;
            }
            segments.push(seg);
        }
        (Index::from_segments(segments), stats)
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn size_bytes(&self) -> usize {
        self.segments.iter().map(Segment::size_bytes).sum()
    }

    pub fn doc_count(&self) -> u64 {
        self.segments.iter().map(|s| s.meta().doc_count).sum()
    }

    /// Document frequency of `term` across all segments.
    pub fn doc_freq(&self, term: &str) -> u64 {
        self.segments.iter().filter_map(|s| s.term(term)).map(|t| t.doc_count()).sum()
    }

    /// Calls `f` for every match, in tid order.
    pub fn search(&self, plan: &Plan, mut f: impl FnMut(Tid)) {
        for s in &self.segments {
            s.search(plan, &mut f);
        }
    }

    pub fn search_vec(&self, plan: &Plan) -> Vec<Tid> {
        let mut v = Vec::new();
        for s in &self.segments {
            s.collect(plan, &mut v);
        }
        v
    }

    pub fn count(&self, plan: &Plan) -> u64 {
        self.segments.iter().map(|s| s.count(plan)).sum()
    }
}
