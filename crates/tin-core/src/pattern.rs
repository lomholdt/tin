//! Term patterns for identifier search: typo-tolerant matching and n-grams.
//!
//! * [`Osa`] is an FST automaton accepting every term within `k` edits of a
//!   query, where an edit is an insertion, deletion, substitution, or a swap
//!   of two neighbouring characters (optimal string alignment). Plain
//!   Levenshtein counts a swap as two edits, which would miss the most common
//!   typo in typed numbers (`…6018200` → `…6012800`).
//! * [`grams`] yields the character trigrams of a term. Indexes built with
//!   grams store them as extra terms under [`GRAM_MARK`], so a fragment
//!   (`*1234565*`) becomes an AND over its trigrams: a candidate set that a
//!   final `contains` check (the heap recheck in Postgres) makes exact.
//!
//! Distances are over bytes; terms are case- and accent-folded already, and
//! identifiers are ASCII.

use fst::Automaton;

/// Prefix byte of gram terms. The analyzer never emits control characters,
/// so gram terms can't collide with real ones.
pub const GRAM_MARK: char = '\u{1}';

/// Fragments shorter than this can't use grams.
pub const GRAM_LEN: usize = 3;

/// Calls `f` with each distinct trigram term (`GRAM_MARK` + 3 chars) of
/// `term`, in order of first appearance.
pub fn grams(term: &str, mut f: impl FnMut(&str)) {
    let chars: Vec<char> = term.chars().collect();
    if chars.len() < GRAM_LEN {
        return;
    }
    let mut seen: Vec<String> = Vec::with_capacity(chars.len());
    let mut buf = String::with_capacity(16);
    for w in chars.windows(GRAM_LEN) {
        buf.clear();
        buf.push(GRAM_MARK);
        buf.extend(w);
        if !seen.contains(&buf) {
            f(&buf);
            seen.push(buf.clone());
        }
    }
}

/// Whether `a` and `b` are within `k` OSA edits.
pub fn osa_within(a: &str, b: &str, k: u8) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len().abs_diff(b.len()) > k as usize {
        return false;
    }
    let aut = Osa::new(b, k);
    let mut s = aut.start();
    for &c in a {
        if !aut.can_match(&s) {
            return false;
        }
        s = aut.accept(&s, c);
    }
    aut.is_match(&s)
}

/// Accepts terms within `k` OSA edits of `query`.
pub struct Osa {
    query: Vec<u8>,
    k: u8,
}

/// Dynamic-programming rows for the term prefix read so far: `cur[j]` is the
/// distance between that prefix and `query[..j]` (capped at k+1), `prev` the
/// row before, `last` the previous term byte (for swaps).
#[derive(Clone, Debug)]
pub struct OsaState {
    cur: Box<[u8]>,
    prev: Option<Box<[u8]>>,
    last: u8,
}

impl Osa {
    pub fn new(query: &[u8], k: u8) -> Self {
        Osa { query: query.to_vec(), k }
    }
}

impl Automaton for Osa {
    type State = OsaState;

    fn start(&self) -> OsaState {
        let cap = self.k + 1;
        let cur = (0..=self.query.len()).map(|j| (j.min(cap as usize)) as u8).collect();
        OsaState { cur, prev: None, last: 0 }
    }

    fn is_match(&self, s: &OsaState) -> bool {
        s.cur[self.query.len()] <= self.k
    }

    fn can_match(&self, s: &OsaState) -> bool {
        s.cur.iter().any(|&d| d <= self.k)
    }

    fn accept(&self, s: &OsaState, c: u8) -> OsaState {
        let cap = self.k + 1;
        let q = &self.query;
        let mut next = vec![0u8; q.len() + 1].into_boxed_slice();
        next[0] = (s.cur[0] + 1).min(cap);
        for j in 1..=q.len() {
            let sub = s.cur[j - 1] + (q[j - 1] != c) as u8;
            let del = s.cur[j] + 1;
            let ins = next[j - 1] + 1;
            let mut d = sub.min(del).min(ins);
            if let Some(prev) = &s.prev {
                if j >= 2 && c == q[j - 2] && s.last == q[j - 1] {
                    d = d.min(prev[j - 2] + 1);
                }
            }
            next[j] = d.min(cap);
        }
        OsaState { cur: next, prev: Some(s.cur.clone()), last: c }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference OSA distance.
    #[allow(clippy::needless_range_loop)]
    fn osa(a: &[u8], b: &[u8]) -> usize {
        let (n, m) = (a.len(), b.len());
        let mut d = vec![vec![0usize; m + 1]; n + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for j in 0..=m {
            d[0][j] = j;
        }
        for i in 1..=n {
            for j in 1..=m {
                let cost = (a[i - 1] != b[j - 1]) as usize;
                d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
                }
            }
        }
        d[n][m]
    }

    #[test]
    fn automaton_matches_reference_distance() {
        let words = [
            "msku6018200",
            "msku6012800",
            "msku6018201",
            "msku601820",
            "msku60182000",
            "mksu6018200",
            "msku6108200",
            "abc",
            "acb",
            "ab",
            "",
            "bac",
            "cab",
            "abcd",
            "badc",
            "12345",
            "13245",
        ];
        for a in words {
            for b in words {
                let d = osa(a.as_bytes(), b.as_bytes());
                for k in 0..=2u8 {
                    assert_eq!(osa_within(a, b, k), d <= k as usize, "{a:?} vs {b:?} k={k} (d={d})");
                }
            }
        }
    }

    #[test]
    fn grams_are_distinct_trigrams() {
        let mut g = Vec::new();
        grams("aaaab", |t| g.push(t.to_owned()));
        assert_eq!(g, ["\u{1}aaa", "\u{1}aab"]);
        g.clear();
        grams("ab", |t| g.push(t.to_owned()));
        assert!(g.is_empty());
        g.clear();
        grams("æøå1", |t| g.push(t.to_owned()));
        assert_eq!(g, ["\u{1}æøå", "\u{1}øå1"]);
    }
}
