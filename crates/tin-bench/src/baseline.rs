//! Reference engine: a textbook inverted index with *uncompressed* sorted
//! posting arrays (tid keys as `u64`) and merge / galloping set operations.
//! Built independently of tin-core's postings, so it doubles as a correctness
//! oracle at full corpus scale.

use std::borrow::Cow;

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use tin_core::{Analyzer, Plan, Tid};

pub struct Baseline {
    postings: FxHashMap<Box<str>, Vec<u64>>,
}

impl Baseline {
    pub fn build(docs: &[(Tid, &str)], chunks: usize) -> Self {
        let size = docs.len().div_ceil(chunks.max(1)).max(1);
        let parts: Vec<FxHashMap<Box<str>, Vec<u64>>> = docs
            .par_chunks(size)
            .map(|chunk| {
                let mut a = Analyzer::new();
                let mut m: FxHashMap<Box<str>, Vec<u64>> = FxHashMap::default();
                for (tid, text) in chunk {
                    let k = tid.key();
                    a.for_each_term(text, |t, _| {
                        let list = match m.get_mut(t) {
                            Some(l) => l,
                            None => m.entry(t.into()).or_default(),
                        };
                        if list.last() != Some(&k) {
                            list.push(k);
                        }
                    });
                }
                m
            })
            .collect();
        // Chunks are in tid order, so appending keeps every list sorted.
        let mut postings: FxHashMap<Box<str>, Vec<u64>> = FxHashMap::default();
        for part in parts {
            for (t, mut l) in part {
                postings.entry(t).or_default().append(&mut l);
            }
        }
        Baseline { postings }
    }

    pub fn bytes(&self) -> usize {
        self.postings.values().map(|l| l.len() * 8).sum()
    }

    /// Union of the postings of every term satisfying `pred` (a full scan).
    fn union_where(&self, pred: impl Fn(&str) -> bool) -> Cow<'_, [u64]> {
        let mut v: Vec<u64> =
            self.postings.iter().filter(|(t, _)| pred(t)).flat_map(|(_, l)| l.iter().copied()).collect();
        v.sort_unstable();
        v.dedup();
        Cow::Owned(v)
    }

    pub fn eval(&self, plan: &Plan) -> Cow<'_, [u64]> {
        match plan {
            Plan::Term(t) => Cow::Borrowed(self.postings.get(t.as_str()).map_or(&[][..], |v| v)),
            Plan::Prefix(p) => self.union_where(|t| t.starts_with(p.as_str())),
            Plan::Fragment(f) => self.union_where(|t| t.contains(f.as_str())),
            Plan::Fuzzy(q, k) => self.union_where(|t| tin_core::pattern::osa_within(t, q, *k)),
            Plan::And(cs) => {
                let mut sets: Vec<_> = cs.iter().map(|c| self.eval(c)).collect();
                sets.sort_by_key(|s| s.len());
                let mut it = sets.into_iter();
                let mut acc = it.next().unwrap();
                for s in it {
                    if acc.is_empty() {
                        break;
                    }
                    acc = Cow::Owned(intersect(&acc, &s));
                }
                acc
            }
            Plan::Or(cs) => {
                let mut it = cs.iter().map(|c| self.eval(c));
                let mut acc = it.next().unwrap();
                for s in it {
                    acc = Cow::Owned(union(&acc, &s));
                }
                acc
            }
            Plan::AndNot(p, n) => {
                let p = self.eval(p);
                let n = self.eval(n);
                Cow::Owned(difference(&p, &n))
            }
        }
    }
}

fn intersect(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(small.len());
    if large.len() > 32 * small.len() {
        // Galloping: binary-search each small element in the shrinking tail.
        let mut lo = 0;
        for &x in small {
            match large[lo..].binary_search(&x) {
                Ok(i) => {
                    out.push(x);
                    lo += i + 1;
                }
                Err(i) => lo += i,
            }
            if lo >= large.len() {
                break;
            }
        }
    } else {
        let (mut i, mut j) = (0, 0);
        while i < small.len() && j < large.len() {
            match small[i].cmp(&large[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(small[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    out
}

fn union(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn difference(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for &x in a {
        while j < b.len() && b[j] < x {
            j += 1;
        }
        if j >= b.len() || b[j] != x {
            out.push(x);
        }
    }
    out
}
