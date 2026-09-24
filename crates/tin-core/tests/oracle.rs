//! Randomized differential tests: every query result must equal what a naive
//! `BTreeSet` evaluator computes from the same documents.

use std::collections::{BTreeMap, BTreeSet};

use tin_core::postings::Encoding;
use tin_core::{Analyzer, Index, Merge, Plan, Segment, SegmentBuilder, Tid};

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
        Plan::Prefix(p) => union_where(truth, |t| t.starts_with(p.as_str())),
        Plan::Fragment(f) => union_where(truth, |t| t.contains(f.as_str())),
        Plan::Fuzzy(q, k) => union_where(truth, |t| osa(t.as_bytes(), q.as_bytes()) <= *k as usize),
        Plan::And(cs) => {
            let mut it = cs.iter().map(|c| eval(c, truth));
            let first = it.next().unwrap();
            it.fold(first, |acc, s| &acc & &s)
        }
        Plan::Or(cs) => cs.iter().map(|c| eval(c, truth)).fold(BTreeSet::new(), |acc, s| &acc | &s),
        Plan::AndNot(p, n) => &eval(p, truth) - &eval(n, truth),
    }
}

fn union_where(truth: &BTreeMap<String, BTreeSet<Tid>>, pred: impl Fn(&str) -> bool) -> BTreeSet<Tid> {
    truth.iter().filter(|(t, _)| pred(t)).flat_map(|(_, s)| s.iter().copied()).collect()
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

#[test]
fn open_ended_segments_and_single_doc_matcher_agree() {
    let c = corpus(13);
    // Cut into open-ended segments every ~500 docs at block boundaries.
    let mut segments = Vec::new();
    let mut b = SegmentBuilder::open_ended(0);
    let mut last_block = 0;
    for (tid, text) in &c.docs {
        if b.doc_count() >= 500 && tid.block != last_block {
            segments.push(b.finish());
            b = SegmentBuilder::open_ended(tid.block);
        }
        b.add(*tid, text);
        last_block = tid.block;
    }
    segments.push(b.finish());
    assert!(segments.len() > 3);
    let idx = Index::from_segments(segments);

    let mut rng = Rng(5);
    let mut a = Analyzer::new();
    for _ in 0..150 {
        let plan = random_plan(&mut rng, 3);
        let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().collect();
        assert_eq!(idx.search_vec(&plan), want, "{plan:?}");
        let scanned: Vec<Tid> =
            c.docs.iter().filter(|(_, text)| plan.matches_text(text, &mut a)).map(|(t, _)| *t).collect();
        assert_eq!(scanned, want, "matches_text {plan:?}");
    }
}

#[test]
fn liveness_bitmaps_hide_deleted_tuples() {
    let c = corpus(21);
    let mut b = SegmentBuilder::new(0, u32::MAX);
    for (tid, text) in &c.docs {
        b.add(*tid, text);
    }
    let seg = b.finish();

    // Every doc maps to a distinct set bit, and back.
    let mut from_bits = Vec::new();
    seg.for_each_set_tid(seg.docs(), |bit, tid| {
        assert_eq!(seg.tid_bit(tid), Some(bit));
        from_bits.push(tid);
    });
    let all: Vec<Tid> = c.docs.iter().map(|(t, _)| *t).collect();
    assert_eq!(from_bits, all);
    assert!(seg.tid_bit(Tid::new(4_999_999, 1)).is_none());

    // Delete a random third of the docs via the liveness bitmap.
    let mut rng = Rng(8);
    let mut live = seg.docs().to_vec();
    let mut dead = BTreeSet::new();
    for (tid, _) in &c.docs {
        if rng.chance(0.33) {
            let bit = seg.tid_bit(*tid).unwrap() as usize;
            live[bit >> 6] &= !(1 << (bit & 63));
            dead.insert(*tid);
        }
    }
    for _ in 0..200 {
        let plan = random_plan(&mut rng, 3);
        let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().filter(|t| !dead.contains(t)).collect();
        let mut got = Vec::new();
        seg.collect_live(&plan, Some(&live), &mut got);
        assert_eq!(got, want, "{plan:?}");
        assert_eq!(seg.count_live(&plan, Some(&live)), want.len() as u64);
        let mut streamed = Vec::new();
        seg.search_live(&plan, Some(&live), |t| streamed.push(t));
        assert_eq!(streamed, want);
    }
}

#[test]
fn add_terms_and_overlapping_segments() {
    let c = corpus(33);
    let mut a = Analyzer::new();
    // Deal docs round-robin into three segments whose block ranges overlap,
    // feeding pre-analyzed terms (as a pending-list flush does).
    let mut builders: Vec<SegmentBuilder> = (0..3).map(|_| SegmentBuilder::open_ended(0)).collect();
    for (i, (tid, text)) in c.docs.iter().enumerate() {
        let terms = a.unique_terms(text);
        builders[i % 3].add_terms(*tid, terms.iter().map(|s| s.as_str()));
    }
    let idx = Index::from_segments(builders.into_iter().map(|b| b.finish()).collect());

    let mut rng = Rng(34);
    for _ in 0..200 {
        let plan = random_plan(&mut rng, 3);
        let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().collect();
        let mut got = idx.search_vec(&plan);
        got.sort();
        assert_eq!(got, want, "{plan:?}");
        assert_eq!(idx.count(&plan), want.len() as u64);
    }
}

#[test]
fn merge_keeps_only_live_tuples() {
    let c = corpus(55);
    let mut a = Analyzer::new();
    // Three overlapping segments (round-robin), each with some deletions.
    let mut builders: Vec<SegmentBuilder> = (0..3).map(|_| SegmentBuilder::open_ended(0)).collect();
    for (i, (tid, text)) in c.docs.iter().enumerate() {
        let terms = a.unique_terms(text);
        builders[i % 3].add_terms(*tid, terms.iter().map(|s| s.as_str()));
    }
    let segs: Vec<Segment> = builders.into_iter().map(|b| b.finish()).collect();
    let mut rng = Rng(56);
    let mut dead = BTreeSet::new();
    let lives: Vec<Vec<u64>> = segs
        .iter()
        .map(|s| {
            let mut live = s.docs().to_vec();
            s.for_each_set_tid(s.docs(), |bit, tid| {
                if rng.chance(0.4) {
                    live[(bit >> 6) as usize] &= !(1 << (bit & 63));
                    dead.insert(tid);
                }
            });
            live
        })
        .collect();
    let inputs: Vec<(&Segment, Option<&[u64]>)> =
        segs.iter().zip(&lives).map(|(s, l)| (s, Some(l.as_slice()))).collect();
    let merged = Segment::merge(&inputs).unwrap();
    assert_eq!(merged.meta().doc_count as usize, c.docs.len() - dead.len());

    // Split by term range, the same bytes however it is cut.
    let whole = merged.to_bytes();
    for n in [2, 3, 7, 1000] {
        let m = Merge::new(&inputs).unwrap();
        let cuts = m.split_points(n);
        assert!(!cuts.is_empty() && cuts.len() < n && cuts.windows(2).all(|w| w[0] < w[1]));
        let bounds: Vec<Option<&[u8]>> =
            std::iter::once(None).chain(cuts.iter().map(|c| Some(c.as_slice()))).chain([None]).collect();
        let parts = bounds.windows(2).map(|w| m.part(w[0], w[1])).collect();
        assert_eq!(m.finish(parts).to_bytes(), whole, "{n} parts");
    }

    // Serialization still round-trips after a merge.
    let merged = Segment::from_bytes(&merged.to_bytes()).unwrap();
    let idx = Index::from_segments(vec![merged]);
    for _ in 0..300 {
        let plan = random_plan(&mut rng, 3);
        let want: Vec<Tid> = eval(&plan, &c.truth).into_iter().filter(|t| !dead.contains(t)).collect();
        assert_eq!(idx.search_vec(&plan), want, "{plan:?}");
    }
    // Everything deleted: nothing to merge.
    let empty: Vec<Vec<u64>> = segs.iter().map(|s| vec![0; s.docs().len()]).collect();
    let inputs: Vec<(&Segment, Option<&[u64]>)> =
        segs.iter().zip(&empty).map(|(s, l)| (s, Some(l.as_slice()))).collect();
    assert!(Segment::merge(&inputs).is_none());
}

/// Identifier-style corpus: each tuple holds 1-3 container/booking-like IDs.
fn id_corpus(seed: u64, n: u32) -> Vec<(Tid, Vec<String>)> {
    let mut rng = Rng(seed);
    let owners = ["msku", "mrku", "maeu", "cmau", "hlxu", "tghu"];
    (0..n)
        .map(|i| {
            let tid = Tid::new(i / 40, (i % 40) as u16 + 1);
            let k = 1 + rng.below(3) as usize;
            let ids = (0..k)
                .map(|_| {
                    if rng.chance(0.5) {
                        format!("{}{:07}", owners[rng.below(6) as usize], rng.below(10_000_000))
                    } else {
                        format!("{:09}", rng.below(1_000_000_000))
                    }
                })
                .collect();
            (tid, ids)
        })
        .collect()
}

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

type Pred = Box<dyn Fn(&str) -> bool>;

#[test]
fn prefix_fragment_and_typo_patterns_match_brute_force() {
    let docs = id_corpus(91, 6000);
    let mut rng = Rng(92);
    for grams in [false, true] {
        let mut b = SegmentBuilder::new(0, u32::MAX).with_grams(grams);
        for (tid, ids) in &docs {
            b.add(*tid, &ids.join(" "));
        }
        let seg = Segment::from_bytes(&b.finish().to_bytes()).unwrap();
        assert_eq!(seg.meta().grams, grams);
        let idx = Index::from_segments(vec![seg]);
        let mut a = Analyzer::new();
        for q in 0..300 {
            let (_, ids) = &docs[rng.below(docs.len() as u64) as usize];
            let id = &ids[rng.below(ids.len() as u64) as usize];
            let (query, pred): (String, Pred) = match q % 5 {
                0 => {
                    let p = id[..3 + rng.below(6) as usize].to_owned();
                    (format!("{p}*"), Box::new(move |t: &str| t.starts_with(&p)))
                }
                1 => {
                    let s = rng.below(4) as usize;
                    let f = id[s..s + 3 + rng.below(4) as usize].to_owned();
                    (format!("*{f}*"), Box::new(move |t: &str| t.contains(&f)))
                }
                2 => {
                    let mut t = id.clone().into_bytes();
                    let i = rng.below(t.len() as u64 - 1) as usize;
                    t.swap(i, i + 1);
                    let t = String::from_utf8(t).unwrap();
                    (format!("{t}~"), Box::new(move |x: &str| osa(x.as_bytes(), t.as_bytes()) <= 1))
                }
                3 => {
                    let mut t = id.clone().into_bytes();
                    t[5] = b'0' + ((t[5] - b'0' + 1) % 10);
                    t.remove(2);
                    let t = String::from_utf8(t).unwrap();
                    (format!("{t}~2"), Box::new(move |x: &str| osa(x.as_bytes(), t.as_bytes()) <= 2))
                }
                _ => {
                    let p = id[..4].to_owned();
                    let f = id[id.len() - 3..].to_owned();
                    (format!("{p}* -*{f}*"), Box::new(move |t: &str| t.starts_with(&p) && !t.contains(&f)))
                }
            };
            let plan = Plan::parse(&query, &mut a).unwrap();
            // Brute force. (The NOT case is a per-document predicate.)
            let want: Vec<Tid> = docs
                .iter()
                .filter(|(_, ids)| {
                    if q % 5 == 4 {
                        let p = &id[..4];
                        let f = &id[id.len() - 3..];
                        ids.iter().any(|t| t.starts_with(p)) && !ids.iter().any(|t| t.contains(f))
                    } else {
                        ids.iter().any(|t| pred(t))
                    }
                })
                .map(|(t, _)| *t)
                .collect();
            let got = idx.search_vec(&plan);
            if plan.needs_recheck() && grams {
                // Candidates: a superset that the recheck narrows to the truth.
                let got_set: BTreeSet<Tid> = got.iter().copied().collect();
                assert!(want.iter().all(|t| got_set.contains(t)), "{query}: missing candidates");
                let text: std::collections::HashMap<Tid, String> =
                    docs.iter().map(|(t, ids)| (*t, ids.join(" "))).collect();
                let rechecked: Vec<Tid> =
                    got.into_iter().filter(|t| plan.matches_text(&text[t], &mut a)).collect();
                assert_eq!(rechecked, want, "{query} (grams, after recheck)");
            } else {
                assert_eq!(got, want, "{query} (grams={grams})");
            }
            // The single-document evaluator agrees too.
            let scanned: Vec<Tid> = docs
                .iter()
                .filter(|(_, ids)| plan.matches_text(&ids.join(" "), &mut a))
                .map(|(t, _)| *t)
                .collect();
            assert_eq!(scanned, want, "{query} matches_text");
        }
    }
}

#[test]
fn patterns_combined_with_other_conditions() {
    // Patterns under AND / OR / NOT with other terms: exercises page masks
    // on expanded pattern cursors.
    let docs = id_corpus(93, 8000);
    let mut rng = Rng(94);
    let mut b = SegmentBuilder::new(0, u32::MAX).with_grams(true);
    for (tid, ids) in &docs {
        b.add(*tid, &ids.join(" "));
    }
    let idx = Index::from_segments(vec![b.finish()]);
    let mut a = Analyzer::new();
    let text: std::collections::HashMap<Tid, String> =
        docs.iter().map(|(t, ids)| (*t, ids.join(" "))).collect();
    let mut nonempty = 0;
    for q in 0..400 {
        let (_, ids) = &docs[rng.below(docs.len() as u64) as usize];
        let x = &ids[rng.below(ids.len() as u64) as usize];
        let y = &ids[rng.below(ids.len() as u64) as usize];
        let other = &docs[rng.below(docs.len() as u64) as usize].1[0];
        let query = match q % 5 {
            0 => format!("{}* {}", &x[..4], y),
            1 => format!("{}~ {}", x, y),
            2 => format!("({}* OR {}~) {}*", &x[..5], other, &y[..3]),
            3 => format!("{}* -{}", &x[..4], y),
            _ => format!("*{}* {}*", &x[x.len() - 4..], &y[..4]),
        };
        let plan = Plan::parse(&query, &mut a).unwrap();
        let want: Vec<Tid> =
            docs.iter().filter(|(t, _)| plan.matches_text(&text[t], &mut a)).map(|(t, _)| *t).collect();
        nonempty += !want.is_empty() as usize;
        let got: Vec<Tid> =
            idx.search_vec(&plan).into_iter().filter(|t| plan.matches_text(&text[t], &mut a)).collect();
        assert_eq!(got, want, "{query}");
        if !plan.needs_recheck() {
            assert_eq!(idx.search_vec(&plan), want, "{query} (exact)");
        }
    }
    assert!(nonempty > 300, "queries should mostly have matches ({nonempty})");
}

#[test]
fn estimates_are_exact_for_terms_and_bound_patterns() {
    let docs = id_corpus(95, 6000);
    let mut rng = Rng(96);
    let mut b = SegmentBuilder::new(0, u32::MAX).with_grams(true);
    for (tid, ids) in &docs {
        b.add(*tid, &ids.join(" "));
    }
    let seg = b.finish();
    let mut a = Analyzer::new();
    let est = |q: &str, a: &mut Analyzer| seg.estimate(&Plan::parse(q, a).unwrap());
    let count = |q: &str, a: &mut Analyzer| {
        let plan = Plan::parse(q, a).unwrap();
        docs.iter().filter(|(_, ids)| plan.matches_text(&ids.join(" "), a)).count() as f64
    };
    assert_eq!(est("zzzz", &mut a), 0.0);
    for _ in 0..200 {
        let (_, ids) = &docs[rng.below(docs.len() as u64) as usize];
        let id = &ids[rng.below(ids.len() as u64) as usize];
        let (e, c) = (est(id, &mut a), count(id, &mut a));
        assert!((e - c).abs() < 1e-6, "{id}: estimate {e}, count {c}");
        // Summed over matching terms (and all of them: the corpus is small),
        // so at least the true count.
        for q in [format!("{}*", &id[..5]), format!("{id}~")] {
            let (e, c) = (est(&q, &mut a), count(&q, &mut a));
            assert!(e > c - 1e-6 && c >= 1.0, "{q}: estimate {e} < count {c}");
        }
        // Grams as if independent: positive, at most the rarest one.
        let f = &id[id.len() - 5..];
        let rarest = [&f[..4], &f[1..]].map(|g| seg.term(&format!("\u{1}{g}")).unwrap().doc_count());
        let e = est(&format!("*{f}*"), &mut a);
        assert!(e > 0.0 && e <= *rarest.iter().min().unwrap() as f64 + 1e-6, "*{f}*: {e} vs {rarest:?}");
        // 3-char fragments are answered exactly from grams: summed over
        // them, so at least the true count.
        let f = format!("*{}*", &id[id.len() - 3..]);
        let (e, c) = (est(&f, &mut a), count(&f, &mut a));
        assert!(e > c - 1e-6, "{f}: estimate {e} < count {c}");
        let (e, n) = (est(&format!("{id} -{id}"), &mut a), seg.meta().doc_count as f64);
        assert!(e < 1.0 && e <= n, "{id} -{id}: {e}");
    }
}

#[test]
fn ranked_search_box_matches_brute_force() {
    use tin_core::rank::{PendingDoc, Ranked, Sources, NO_TIER};
    use tin_core::{SearchBox, SortedTerms};

    let docs = id_corpus(97, 6000);
    let mut rng = Rng(98);
    // Two segments over disjoint blocks, the last 300 docs pending.
    let (indexed, pending) = docs.split_at(docs.len() - 300);
    let cut = indexed.len() / 2;
    let seg_of = |part: &[(Tid, Vec<String>)]| {
        let mut b = SegmentBuilder::new(part[0].0.block, part[part.len() - 1].0.block + 1).with_grams(true);
        for (tid, ids) in part {
            b.add(*tid, &ids.join(" "));
        }
        b.finish()
    };
    let segs = [seg_of(&indexed[..cut]), seg_of(&indexed[cut..])];
    // Delete ~10% of indexed tuples.
    let dead: BTreeSet<Tid> = indexed.iter().filter(|_| rng.chance(0.1)).map(|(t, _)| *t).collect();
    let lives: Vec<Vec<u64>> = segs
        .iter()
        .map(|s| {
            let mut l = s.docs().to_vec();
            for t in &dead {
                if let Some(b) = s.tid_bit(*t) {
                    l[(b >> 6) as usize] &= !(1 << (b & 63));
                }
            }
            l
        })
        .collect();
    let mut a = Analyzer::new();
    let pending_docs: Vec<PendingDoc> = pending
        .iter()
        .map(|(t, ids)| PendingDoc { tid: *t, terms: a.unique_terms(&ids.join(" ")) })
        .collect();
    let src = Sources {
        segments: segs.iter().zip(&lives).map(|(s, l)| (s, Some(l.as_slice()))).collect(),
        pending: &pending_docs,
    };
    let live_docs: Vec<(Tid, Vec<String>)> = docs
        .iter()
        .filter(|(t, _)| !dead.contains(t))
        .map(|(t, ids)| (*t, a.unique_terms(&ids.join(" "))))
        .collect();

    let mut streamed = 0;
    for q in 0..300 {
        let (_, ids) = &docs[rng.below(docs.len() as u64) as usize];
        let id = ids[0].clone();
        let other = &docs[rng.below(docs.len() as u64) as usize].1[0];
        let text = match q % 7 {
            0 => id.clone(),
            1 => id[..3 + rng.below(4) as usize].to_owned(),
            2 => id[id.len() - 5..].to_owned(),
            3 => {
                let mut t = id.clone().into_bytes();
                let i = rng.below(t.len() as u64 - 1) as usize;
                t.swap(i, i + 1);
                String::from_utf8(t).unwrap()
            }
            4 => format!("{}x", &id[..id.len() - 1]),
            5 => format!("{} {}", &id[..4], other),
            _ => id[..2].to_owned(),
        };
        let query = SearchBox::parse(&text, &mut a).with_max_typos((q / 3 % 3) as u8);
        let filter = (q % 3 == 0).then(|| Plan::parse(&format!("{}*", &other[..2]), &mut a).unwrap());
        streamed += (filter.is_none() && query.terms().len() == 1) as usize;

        let mut r = Ranked::new(query.clone(), filter.clone());
        let mut seen = BTreeSet::new();
        let mut got = BTreeSet::new();
        let mut last_bound = 0;
        let terms_of: std::collections::HashMap<Tid, &Vec<String>> =
            live_docs.iter().map(|(t, v)| (*t, v)).collect();
        while let Some(h) = r.next(&src) {
            assert!(seen.insert(h.tid), "{text:?}: {:?} twice", h.tid);
            assert!(h.tier >= last_bound, "{text:?}: tiers out of order");
            last_bound = h.tier;
            let terms =
                terms_of.get(&h.tid).unwrap_or_else(|| panic!("{text:?}: dead or unknown {:?}", h.tid));
            let true_tier = query.tier_of_terms(terms).unwrap_or(NO_TIER);
            let passes = filter.as_ref().is_none_or(|f| f.matches(&SortedTerms(terms)))
                && (filter.is_some() || true_tier < NO_TIER);
            if !h.recheck {
                assert!(passes, "{text:?}: {:?} doesn't match", h.tid);
            }
            // Any tier but the true one must be flagged as a lower bound, so
            // the executor recomputes it.
            if h.tier_is_bound {
                assert!(h.tier <= true_tier, "{text:?}: bound {} > tier {true_tier}", h.tier);
            } else {
                assert_eq!(h.tier, true_tier, "{text:?}: {:?}", h.tid);
            }
            if passes {
                got.insert(h.tid);
            }
        }
        let want: BTreeSet<Tid> = live_docs
            .iter()
            .filter(|(_, terms)| match &filter {
                Some(f) => f.matches(&SortedTerms(terms)),
                None => query.tier_of_terms(terms).is_some(),
            })
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(got, want, "{text:?} filter {filter:?}");
    }
    assert!(streamed > 100);
}
