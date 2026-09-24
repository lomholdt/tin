//! Positional evaluation of a [`Query`] against one document: the exact
//! answer for phrases, `THEN/N` / `NEAR/N` and `AT LEAST`, which the index
//! only narrows down to candidates.
//!
//! Every query node denotes the set of **minimal intervals** of word
//! positions where it is satisfied (Clarke–Cormack–Burkowski; the same
//! algebra as Boldi and Vigna's "efficient lazy algorithms for minimal-interval
//! semantics"): an interval is kept only if it contains no other one. A term
//! gives one interval per occurrence; `AND` gives the minimal windows holding
//! one interval of each operand; `OR` the minimal intervals of the union;
//! a phrase or `THEN/N` the minimal windows with the operands in order and
//! close enough. A document matches if the root's set is non-empty.
//!
//! Negation is document-level: `a AND NOT b` has `a`'s intervals if `b` has
//! none. This evaluator works on one row at a time (rechecks), so it favours
//! simple sorted-vector code over lazy iterators.

use rustc_hash::FxHashMap;

use crate::pattern::osa_within;
use crate::query::{Query, Slot};
use crate::tokenize::Analyzer;

/// A closed interval of word positions.
pub type Span = (u32, u32);

/// One document's words: each distinct term with its positions.
pub struct DocWords {
    terms: FxHashMap<String, Vec<u32>>,
    words: u32,
}

impl DocWords {
    pub fn new(text: &str, analyzer: &mut Analyzer) -> DocWords {
        let mut terms: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        let mut words = 0;
        analyzer.for_each_term(text, |t, pos| {
            words = pos + 1;
            match terms.get_mut(t) {
                Some(v) => v.push(pos),
                None => {
                    terms.insert(t.to_owned(), vec![pos]);
                }
            }
        });
        DocWords { terms, words }
    }

    /// Distinct terms.
    pub fn distinct_terms(&self) -> usize {
        self.terms.len()
    }

    /// Word positions in the document (dropped over-long words included).
    pub fn words(&self) -> u32 {
        self.words
    }

    /// Positions of `term`.
    pub fn positions(&self, term: &str) -> &[u32] {
        self.terms.get(term).map_or(&[], |v| v.as_slice())
    }

    /// Every (term, positions) matched by a leaf (a term or pattern).
    pub fn leaf_terms<'a>(&'a self, leaf: &'a Query) -> Box<dyn Iterator<Item = (&'a str, &'a [u32])> + 'a> {
        let hit = |t: &str| -> bool {
            match leaf {
                Query::Prefix(p) => t.starts_with(p.as_str()),
                Query::Fragment(f) => t.contains(f.as_str()),
                Query::Fuzzy(q, k) => osa_within(t, q, *k),
                _ => false,
            }
        };
        match leaf {
            Query::Term(t) => {
                Box::new(self.terms.get_key_value(t).map(|(k, v)| (k.as_str(), v.as_slice())).into_iter())
            }
            _ => Box::new(
                self.terms.iter().filter(move |(t, _)| hit(t)).map(|(k, v)| (k.as_str(), v.as_slice())),
            ),
        }
    }

    pub fn matches(&self, q: &Query) -> bool {
        !self.spans(q).is_empty()
    }

    /// The minimal intervals of `q`, sorted (by start and by end).
    pub fn spans(&self, q: &Query) -> Vec<Span> {
        match q {
            Query::Term(_) | Query::Prefix(_) | Query::Fragment(_) | Query::Fuzzy(..) => self.leaf_spans(q),
            Query::Boost(q, _) => self.spans(q),
            // Only reachable as the root of a pure negation, which the parser
            // rejects; as a document-level test it has no intervals.
            Query::Not(_) => Vec::new(),
            Query::Or(cs) => {
                let mut all: Vec<Span> = cs.iter().flat_map(|c| self.spans(c)).collect();
                minimize(&mut all);
                all
            }
            Query::And(cs) => {
                let mut lists = Vec::new();
                for c in cs {
                    match c {
                        Query::Not(n) => {
                            if self.matches(n) {
                                return Vec::new();
                            }
                        }
                        c => lists.push(self.spans(c)),
                    }
                }
                let n = lists.len();
                cover(&lists, n)
            }
            Query::AtLeast { min, of } => {
                let lists: Vec<Vec<Span>> = of.iter().map(|c| self.spans(c)).collect();
                cover(&lists, *min)
            }
            Query::Phrase { slots, slop } => self.phrase(slots, *slop),
            Query::Near { left, right, gap, ordered } => {
                let (l, r) = (self.spans(left), self.spans(right));
                let mut out = then(&l, &r, *gap);
                if !ordered {
                    out.extend(then(&r, &l, *gap));
                    minimize(&mut out);
                }
                out
            }
        }
    }

    fn leaf_spans(&self, leaf: &Query) -> Vec<Span> {
        let mut v: Vec<Span> = self.leaf_terms(leaf).flat_map(|(_, ps)| ps.iter().map(|&p| (p, p))).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Positions matched by any alternative of a slot, sorted.
    fn slot_positions(&self, slot: &Slot) -> Vec<u32> {
        let mut v: Vec<u32> = slot.alts.iter().flat_map(|a| self.leaf_spans(a)).map(|(p, _)| p).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Each slot at its offset from the previous one or later, with at most
    /// `slop` extra words in all. Taking the earliest position for each slot
    /// minimizes both the extra words so far and the constraint on the next
    /// slot, so one greedy pass per first position finds the shortest match
    /// starting there.
    fn phrase(&self, slots: &[Slot], slop: u32) -> Vec<Span> {
        let pos: Vec<Vec<u32>> = slots.iter().map(|s| self.slot_positions(s)).collect();
        if pos.iter().any(Vec::is_empty) {
            return Vec::new();
        }
        let mut out = Vec::new();
        'start: for &p0 in &pos[0] {
            let (mut cur, mut extra) = (p0, 0u32);
            for j in 1..slots.len() {
                let need = cur + (slots[j].offset - slots[j - 1].offset);
                let k = pos[j].partition_point(|&p| p < need);
                let Some(&p) = pos[j].get(k) else { break 'start };
                extra += p - need;
                if extra > slop {
                    continue 'start;
                }
                cur = p;
            }
            out.push((p0, cur));
        }
        minimize(&mut out);
        out
    }
}

/// Keep only intervals containing no other, sorted by start (and end).
pub fn minimize(v: &mut Vec<Span>) {
    // By end, then latest start first: an interval is minimal iff every
    // interval before it starts earlier.
    v.sort_unstable_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
    let mut max_start: Option<u32> = None;
    v.retain(|&(s, _)| {
        let keep = max_start.is_none_or(|m| s > m);
        max_start = Some(max_start.map_or(s, |m| m.max(s)));
        keep
    });
}

/// The minimal windows holding an interval of at least `k` of `lists` (each
/// minimal, so sorted by start and end alike).
fn cover(lists: &[Vec<Span>], k: usize) -> Vec<Span> {
    if k == 0 || lists.iter().filter(|l| !l.is_empty()).count() < k {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut ends = Vec::with_capacity(lists.len());
    // A minimal window starts where one of its intervals starts; from each
    // list, the first interval starting there or later ends soonest.
    for (i, list) in lists.iter().enumerate() {
        for &(s, e) in list {
            ends.clear();
            for (j, other) in lists.iter().enumerate() {
                if j != i {
                    let at = other.partition_point(|&(os, _)| os < s);
                    if let Some(&(_, oe)) = other.get(at) {
                        ends.push(oe);
                    }
                }
            }
            if ends.len() + 1 < k {
                continue;
            }
            ends.sort_unstable();
            let end = ends[..k - 1].iter().fold(e, |m, &x| m.max(x));
            out.push((s, end));
        }
    }
    minimize(&mut out);
    out
}

/// `r` after `l` with at most `gap` words between: for each `l`, the first
/// `r` starting after it ends soonest.
fn then(l: &[Span], r: &[Span], gap: u32) -> Vec<Span> {
    let mut out = Vec::new();
    for &(ls, le) in l {
        let at = r.partition_point(|&(rs, _)| rs <= le);
        if let Some(&(rs, re)) = r.get(at) {
            if rs - le - 1 <= gap {
                out.push((ls, re));
            }
        }
    }
    minimize(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(q: &str, doc: &str) -> bool {
        let mut a = Analyzer::new();
        Query::parse(q, &mut a).unwrap().matches_text(doc, &mut a)
    }

    #[test]
    fn minimal_intervals() {
        let mut v = vec![(0, 5), (1, 3), (2, 3), (4, 6), (4, 6), (7, 7)];
        minimize(&mut v);
        assert_eq!(v, [(2, 3), (4, 6), (7, 7)]);
        let lists = vec![vec![(0, 0), (5, 5)], vec![(2, 2), (9, 9)], vec![(3, 3)]];
        assert_eq!(cover(&lists, 3), [(0, 3), (2, 5), (3, 9)]);
        assert_eq!(cover(&lists, 2), [(0, 2), (2, 3), (3, 5), (5, 9)]);
    }

    #[test]
    fn phrases() {
        let doc = "the big bad wolf ate a big grey wolf";
        assert!(m("\"big bad wolf\"", doc));
        assert!(!m("\"bad big wolf\"", doc));
        assert!(m("\"big _ wolf\"", doc));
        assert!(!m("\"big _ _ wolf\"", doc)); // exactly two words between: none
        assert!(m("\"[big large] grey wolf\"", doc));
        assert!(!m("\"big wolf\"", doc));
        assert!(m("\"big wolf\"~1", doc));
        assert!(m("\"the wolf\"~2", doc));
        assert!(!m("\"the wolf\"~1", doc));
        assert!(m("\"Big-Bad\"", doc)); // one word the analyzer splits
        assert!(m("big-bad", doc));
        assert!(!m("bad-big", doc));
    }

    #[test]
    fn proximity() {
        let doc = "craft pale beer and more beer";
        assert!(m("craft THEN/0 pale", doc));
        assert!(!m("craft THEN/0 beer", doc));
        assert!(m("craft THEN/1 beer", doc));
        assert!(!m("beer THEN/5 craft", doc));
        assert!(m("beer NEAR/1 craft", doc));
        assert!(!m("beer NEAR/0 craft", doc));
        assert!(m("\"craft pale\" THEN/0 beer", doc));
        assert!(m("craft THEN/1 beer THEN/2 beer", doc));
        assert!(!m("craft THEN/1 beer THEN/1 beer", doc));
        assert!(m("[ale beer] NEAR/1 craft", doc));
        assert!(m("(craft AND NOT lager) NEAR/2 beer", doc));
        assert!(!m("(craft AND NOT pale) NEAR/2 beer", doc));
    }

    #[test]
    fn at_least_and_boolean() {
        let doc = "one two three";
        assert!(m("AT LEAST 2 OF [one four three]", doc));
        assert!(!m("AT LEAST 2 OF [one four five]", doc));
        assert!(m("AT LEAST 50% OF [one two four five]", "one two"));
        assert!(!m("AT LEAST 50% OF [one two four five]", "one six"));
        assert!(!m("AT LEAST 60% OF [one two four five]", "one two")); // 3 of 4, rounded up
        assert!(m("ALL OF [one two]", doc));
        assert!(m("one AND NOT \"three two\"", doc));
        assert!(!m("one AND NOT \"two three\"", doc));
        assert!(m("one^2 two", doc));
    }
}
