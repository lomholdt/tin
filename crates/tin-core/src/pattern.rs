//! Term patterns for identifier search: typo-tolerant matching and n-grams.
//!
//! * [`Osa`] is an FST automaton accepting every term within `k` edits of a
//!   query, where an edit is an insertion, deletion, substitution, or a swap
//!   of two neighbouring characters (optimal string alignment). Plain
//!   Levenshtein counts a swap as two edits, which would miss the most common
//!   typo in typed numbers (`…6018200` → `…6012800`).
//! * [`grams`] yields a term's character 4-grams, the last one padded with
//!   [`GRAM_END`]. Indexes built with grams store them as extra terms under
//!   [`GRAM_MARK`]. A fragment of 4+ characters (`*1234565*`) becomes an AND
//!   over its own 4-grams: a candidate set that a final `contains` check (the
//!   heap recheck in Postgres) makes exact. A 3-character fragment begins
//!   some gram of every term containing it (the padding covers the term's
//!   end), so it is an exact prefix search over the grams.
//!
//! Why 4-grams and not trigrams (pg_trgm's choice): identifiers are mostly
//! digits, and there are only 1,000 digit trigrams, so each one is in ~2% of
//! rows and every fragment ANDs long lists. There are 10,000 digit 4-grams,
//! and a term has as many 4-grams (padded) as trigrams.
//!
//! Distances are over bytes; terms are case- and accent-folded already, and
//! identifiers are ASCII.

use fst::Automaton;

/// Prefix byte of gram terms. The analyzer never emits control characters,
/// so gram terms can't collide with real ones.
pub const GRAM_MARK: char = '\u{1}';

/// Pads the end of a term's last gram.
pub const GRAM_END: char = '\u{2}';

/// Characters per gram.
pub const GRAM_LEN: usize = 4;

/// The shortest fragment grams can answer.
pub const GRAM_MIN_FRAGMENT: usize = GRAM_LEN - 1;

/// Calls `f` with each distinct gram term of `term` (`GRAM_MARK` + 4 chars
/// of `term` + `GRAM_END`), in order of first appearance. What an index with
/// grams stores.
pub fn grams(term: &str, f: impl FnMut(&str)) {
    windows(term.chars().chain([GRAM_END]), f)
}

/// The gram terms of every term containing `fragment` (4+ chars).
pub fn fragment_grams(fragment: &str, f: impl FnMut(&str)) {
    windows(fragment.chars(), f)
}

/// For a 3-character `fragment`: the prefix of the gram terms that start
/// with it.
pub fn fragment_gram_prefix(fragment: &str) -> String {
    format!("{GRAM_MARK}{fragment}")
}

fn windows(chars: impl Iterator<Item = char>, mut f: impl FnMut(&str)) {
    let chars: Vec<char> = chars.collect();
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

/// Longest query [`Osa`] can match: terms are at most `MAX_TERM_BYTES`
/// long, so a longer query is more than 2 edits from all of them.
const OSA_MAX: usize = crate::tokenize::MAX_TERM_BYTES + 2;

/// Accepts terms within `k` OSA edits of `query`.
pub struct Osa {
    query: Vec<u8>,
    k: u8,
    /// Query too long to match anything.
    dead: bool,
}

/// Dynamic-programming rows for the term prefix read so far: `cur[j]` is the
/// distance between that prefix and `query[..j]` (capped at k+1), `prev` the
/// row before (if any), `last` the previous term byte (for swaps). Fixed-size,
/// so following an FST edge allocates nothing.
#[derive(Clone, Debug)]
pub struct OsaState {
    cur: [u8; OSA_MAX + 1],
    prev: [u8; OSA_MAX + 1],
    has_prev: bool,
    last: u8,
}

impl Osa {
    pub fn new(query: &[u8], k: u8) -> Self {
        let dead = query.len() > OSA_MAX;
        Osa { query: if dead { Vec::new() } else { query.to_vec() }, k, dead }
    }
}

impl Automaton for Osa {
    type State = OsaState;

    fn start(&self) -> OsaState {
        let cap = self.k + 1;
        let mut cur = [cap; OSA_MAX + 1];
        for (j, d) in cur[..=self.query.len()].iter_mut().enumerate() {
            *d = j.min(cap as usize) as u8;
        }
        OsaState { cur, prev: [cap; OSA_MAX + 1], has_prev: false, last: 0 }
    }

    fn is_match(&self, s: &OsaState) -> bool {
        !self.dead && s.cur[self.query.len()] <= self.k
    }

    fn can_match(&self, s: &OsaState) -> bool {
        !self.dead && s.cur[..=self.query.len()].iter().any(|&d| d <= self.k)
    }

    fn accept(&self, s: &OsaState, c: u8) -> OsaState {
        let cap = self.k + 1;
        let q = &self.query;
        let mut next = OsaState { cur: [cap; OSA_MAX + 1], prev: s.cur, has_prev: true, last: c };
        next.cur[0] = (s.cur[0] + 1).min(cap);
        for j in 1..=q.len() {
            let sub = s.cur[j - 1] + (q[j - 1] != c) as u8;
            let del = s.cur[j] + 1;
            let ins = next.cur[j - 1] + 1;
            let mut d = sub.min(del).min(ins);
            if s.has_prev && j >= 2 && c == q[j - 2] && s.last == q[j - 1] {
                d = d.min(s.prev[j - 2] + 1);
            }
            next.cur[j] = d.min(cap);
        }
        next
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
    fn grams_are_distinct_padded_4grams() {
        let idx = |s: &str| {
            let mut v = Vec::new();
            grams(s, |t| v.push(t.to_owned()));
            v
        };
        let frag = |s: &str| {
            let mut v = Vec::new();
            fragment_grams(s, |t| v.push(t.to_owned()));
            v
        };
        assert_eq!(idx("aaaaab"), ["\u{1}aaaa", "\u{1}aaab", "\u{1}aab\u{2}"]);
        assert_eq!(idx("abc"), ["\u{1}abc\u{2}"]);
        assert!(idx("ab").is_empty());
        assert_eq!(idx("æøå1"), ["\u{1}æøå1", "\u{1}øå1\u{2}"]);
        assert_eq!(frag("12345"), ["\u{1}1234", "\u{1}2345"]);
        assert!(frag("123").is_empty());
        // Every 3-char substring starts some gram of the term.
        let term = "msku6008200";
        let all = idx(term);
        for i in 0..=term.len() - 3 {
            let p = fragment_gram_prefix(&term[i..i + 3]);
            assert!(all.iter().any(|t| t.starts_with(&p)), "{p:?}");
        }
    }
}
