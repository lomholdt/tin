//! Randomized differential tests: every query result must equal what a naive
//! `BTreeSet` evaluator computes from the same documents.

use std::collections::{BTreeMap, BTreeSet};

use tin_core::postings::Encoding;
use tin_core::{Analyzer, Index, Plan, Segment, SegmentBuilder, Tid};

/// SplitMix64 — deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

const VOCAB: usize = 40;

/// Term i appears with probability ~ 0.95 / 1.35^i: from nearly every tuple
/// (dense bitmaps) down to a handful (sparse lists and singletons).
fn term_prob(i: usize) -> f64 {
    0.95 / 1.35f64.powi(i as i32)
}

struct Corpus {
    docs: Vec<(Tid, String)>,
    truth: BTreeMap<String, BTreeSet<Tid>>,
}

fn corpus(seed: u64) -> Corpus {
    let mut rng = Rng(seed);
    let mut tids = Vec::new();
    // Dense region: full-ish pages in one group -> page bitmaps + offset bitmaps.
    for block in 0..60 {
        for off in 1..=rng.below(291) as u16 + 1 {
            tids.push(Tid::new(block, off));
        }
    }
    // Sparse region with holes spanning several groups.
    let mut block = 300;
    while block < 3_000 {
        for off in 1..=rng.below(6) as u16 + 1 {
            tids.push(Tid::new(block, off * 7));
        }
        block += 1 + rng.below(40) as u32;
    }
    // A far-away straggler: large group deltas.
    tids.push(Tid::new(5_000_000, 3));
    tids.push(Tid::new(5_000_000, crate_max_offset()));

    let mut docs = Vec::new();
    let mut truth: BTreeMap<String, BTreeSet<Tid>> = BTreeMap::new();
    for tid in tids {
        let mut words = Vec::new();
        for i in 0..VOCAB {
            if rng.chance(term_prob(i)) {
                let w = format!("t{i}");
                truth.entry(w.clone()).or_default().insert(tid);
                // Repeat some words to exercise per-tuple dedup.
                let reps = 1 + rng.below(2);
                for _ in 0..reps {
                    words.push(w.clone());
                }
            }
        }
        // Mixed case + punctuation must analyze to the same terms.
        let text = words
            .iter()
            .map(|w| if rng.chance(0.3) { w.to_uppercase() } else { w.clone() })
            .collect::<Vec<_>>()
            .join(if rng.chance(0.5) { " " } else { ", " });
        docs.push((tid, text));
    }
    Corpus { docs, truth }
}

fn crate_max_offset() -> u16 {
    tin_core::tid::MAX_OFFSET
}

fn eval(plan: &Plan, truth: &BTreeMap<String, BTreeSet<Tid>>) -> BTreeSet<Tid> {
    match plan {
        Plan::Term(t) => truth.get(t).cloned().unwrap_or_default(),
        Plan::And(cs) => {
            let mut it = cs.iter().map(|c| eval(c, truth));
            let first = it.next().unwrap();
            it.fold(first, |acc, s| &acc & &s)
        }
        Plan::Or(cs) => cs.iter().map(|c| eval(c, truth)).fold(BTreeSet::new(), |acc, s| &acc | &s),
        Plan::AndNot(p, n) => &eval(p, truth) - &eval(n, truth),
    }
}

fn random_plan(rng: &mut Rng, depth: u32) -> Plan {
    let term = |rng: &mut Rng| {
        if rng.chance(0.05) {
            Plan::Term("missing".into())
        } else {
            // Bias towards frequent terms so intersections are non-trivial.
            let i = (rng.below(VOCAB as u64) * rng.below(VOCAB as u64) / VOCAB as u64) as usize;
            Plan::Term(format!("t{i}"))
        }
    };
    if depth == 0 || rng.chance(0.25) {
        return term(rng);
    }
    let n = 2 + rng.below(3) as usize;
    let kids: Vec<Plan> = (0..n).map(|_| random_plan(rng, depth - 1)).collect();
    match rng.below(3) {
        0 => Plan::And(kids),
        1 => Plan::Or(kids),
        _ => {
            let mut kids = kids;
            let neg = kids.pop().unwrap();
            let pos = if kids.len() == 1 { kids.pop().unwrap() } else { Plan::And(kids) };
            Plan::AndNot(Box::new(pos), Box::new(neg))
        }
    }
}

#[test]
fn random_queries_match_oracle() {
    for seed in 1..=4 {
        let c = corpus(seed);
        let indexes: Vec<Index> = [1, 3, 8].iter().map(|&n| Index::build(&c.docs, n)).collect();
        let mut rng = Rng(seed * 1000);
        for q in 0..400 {
            let plan = random_plan(&mut rng, 3);
            let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().collect();
            for (i, idx) in indexes.iter().enumerate() {
                let got = idx.search_vec(&plan);
                assert_eq!(got, want, "seed {seed} query {q} index {i}: {plan:?}");
                let mut streamed = Vec::new();
                idx.search(&plan, |t| streamed.push(t));
                assert_eq!(streamed, want, "streamed seed {seed} query {q}: {plan:?}");
                assert_eq!(idx.count(&plan), want.len() as u64, "count seed {seed} query {q}: {plan:?}");
            }
        }
    }
}

#[test]
fn every_encoding_is_exercised() {
    let c = corpus(7);
    let (_, stats) = Index::build_with_stats(&c.docs, 3, |_, _| ());
    let seen: BTreeSet<Encoding> = stats.keys().map(|&(_, e)| e).collect();
    assert_eq!(seen, BTreeSet::from([Encoding::Singleton, Encoding::Sparse, Encoding::Groups]));
}

#[test]
fn parsed_queries_match_oracle() {
    let c = corpus(11);
    let idx = Index::build(&c.docs, 4);
    let mut a = Analyzer::new();
    for q in ["t0 t1", "T3 OR t9", "(t2 OR t12) -t0", "t0 t1 t2 -(t3 OR t4)", "t30 OR t31 OR t35"] {
        let plan = Plan::parse(q, &mut a).unwrap();
        let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().collect();
        assert_eq!(idx.search_vec(&plan), want, "{q}");
    }
}

#[test]
fn serialization_roundtrip() {
    let c = corpus(3);
    let mut b = SegmentBuilder::new(0, u32::MAX);
    for (tid, text) in &c.docs {
        b.add(*tid, text);
    }
    let seg = b.finish();
    let bytes = seg.to_bytes();
    let back = Segment::from_bytes(&bytes).unwrap();
    assert_eq!(back.meta(), seg.meta());
    assert_eq!(back.to_bytes(), bytes);

    let mut rng = Rng(99);
    let idx = Index::from_segments(vec![back]);
    for _ in 0..100 {
        let plan = random_plan(&mut rng, 2);
        let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().collect();
        assert_eq!(idx.search_vec(&plan), want);
    }

    assert!(Segment::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert!(Segment::from_bytes(&bad).is_err());
}

#[test]
fn doc_freq_sums_segments() {
    let c = corpus(5);
    let idx = Index::build(&c.docs, 5);
    for (term, set) in &c.truth {
        assert_eq!(idx.doc_freq(term), set.len() as u64, "{term}");
    }
    assert_eq!(idx.doc_count(), c.docs.len() as u64);
}

/// Tuples spread one-per-group so mid-frequency terms choose the sparse
/// encoding with long, skip-blocked lists; AND with rarer terms must jump.
#[test]
fn skip_blocked_sparse_lists_match_oracle() {
    let mut rng = Rng(77);
    let mut docs = Vec::new();
    let mut truth: BTreeMap<String, BTreeSet<Tid>> = BTreeMap::new();
    for i in 0..12_000u32 {
        let tid = Tid::new(i * 257 + rng.below(3) as u32, 1 + rng.below(4) as u16);
        let mut words = Vec::new();
        for t in 0..12 {
            if rng.chance(0.6 / 1.6f64.powi(t)) {
                let w = format!("s{t}");
                truth.entry(w.clone()).or_default().insert(tid);
                words.push(w);
            }
        }
        docs.push((tid, words.join(" ")));
    }
    let (idx, stats) = Index::build_with_stats(&docs, 3, |df, _| df > tin_core::postings::SKIP_BLOCK as u64);
    let long_sparse = stats.get(&(true, Encoding::Sparse)).map_or(0, |s| s.terms);
    assert!(long_sparse >= 5, "want several skip-blocked sparse lists, got {long_sparse}");

    let mut q = 0;
    for a in 0..12 {
        for b in 0..12 {
            for plan in [
                Plan::And(vec![Plan::Term(format!("s{a}")), Plan::Term(format!("s{b}"))]),
                Plan::AndNot(Box::new(Plan::Term(format!("s{a}"))), Box::new(Plan::Term(format!("s{b}")))),
                Plan::Or(vec![Plan::Term(format!("s{a}")), Plan::Term(format!("s{b}"))]),
            ] {
                let want: Vec<Tid> = eval(&plan, &truth).into_iter().collect();
                assert_eq!(idx.search_vec(&plan), want, "{plan:?}");
                q += 1;
            }
        }
    }
    assert_eq!(q, 432);
}
