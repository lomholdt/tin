//! Randomized differential test for positional queries (phrases, gaps,
//! slop, THEN/N, NEAR/N, AT LEAST, boolean mixes): random documents and
//! random queries, rendered to query text and parsed, then checked against
//! a brute-force evaluator written straight from the definitions (it
//! enumerates positions and never computes minimal intervals). Also checks
//! that the index plan never misses a match, and that the index agrees with
//! the plan.

use tin_core::{Analyzer, Index, Plan, Query, SegmentBuilder, SortedTerms, Tid};

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
}

const WORDS: [&str; 6] = ["ale", "beer", "craft", "dark", "ember", "fox"];

/// A query as generated: rendered to text for the parser, and evaluated by
/// brute force.
#[derive(Clone, Debug)]
enum G {
    Word(usize),
    /// (slot words, offset of each slot), slop. Each slot: alternatives.
    Phrase(Vec<(Vec<usize>, u32)>, u32),
    Near(Box<G>, Box<G>, u32, bool),
    AtLeast(usize, Vec<G>),
    And(Vec<G>, Vec<G>),
    Or(Vec<G>),
}

impl G {
    fn render(&self) -> String {
        match self {
            G::Word(w) => WORDS[*w].to_owned(),
            G::Phrase(slots, slop) => {
                let mut s = String::from("\"");
                let mut at = 0;
                for (alts, off) in slots {
                    while at < *off {
                        s.push_str("_ ");
                        at += 1;
                    }
                    if alts.len() == 1 {
                        s.push_str(WORDS[alts[0]]);
                    } else {
                        s.push('[');
                        s.push_str(&alts.iter().map(|&w| WORDS[w]).collect::<Vec<_>>().join(", "));
                        s.push(']');
                    }
                    s.push(' ');
                    at += 1;
                }
                s.pop();
                s.push('"');
                if *slop > 0 {
                    s.push_str(&format!("~{slop}"));
                }
                s
            }
            G::Near(l, r, gap, ordered) => {
                format!("({} {}/{gap} {})", l.render(), if *ordered { "THEN" } else { "NEAR" }, r.render())
            }
            G::AtLeast(k, of) => {
                format!("AT LEAST {k} OF [{}]", of.iter().map(G::render).collect::<Vec<_>>().join(" "))
            }
            G::And(pos, neg) => {
                let mut parts: Vec<String> = pos.iter().map(G::render).collect();
                parts.extend(neg.iter().map(|n| format!("AND NOT {}", n.render())));
                format!("({})", parts.join(" "))
            }
            G::Or(cs) => format!("({})", cs.iter().map(G::render).collect::<Vec<_>>().join(" OR ")),
        }
    }

    /// Every (start, end) where this matches, by brute force. Only asked of
    /// words and phrases.
    fn spans(&self, doc: &[usize]) -> Vec<(usize, usize)> {
        match self {
            G::Word(w) => (0..doc.len()).filter(|&i| doc[i] == *w).map(|i| (i, i)).collect(),
            G::Phrase(slots, slop) => {
                let mut out = Vec::new();
                #[allow(clippy::too_many_arguments)]
                fn rec(
                    doc: &[usize],
                    slots: &[(Vec<usize>, u32)],
                    j: usize,
                    prev: usize,
                    extra: u32,
                    slop: u32,
                    start: usize,
                    out: &mut Vec<(usize, usize)>,
                ) {
                    if j == slots.len() {
                        out.push((start, prev));
                        return;
                    }
                    let d = (slots[j].1 - slots[j - 1].1) as usize;
                    for p in prev + d..doc.len() {
                        let e = extra + (p - prev - d) as u32;
                        if e > slop {
                            break;
                        }
                        if slots[j].0.contains(&doc[p]) {
                            rec(doc, slots, j + 1, p, e, slop, start, out);
                        }
                    }
                }
                for p in 0..doc.len() {
                    if slots[0].0.contains(&doc[p]) {
                        rec(doc, slots, 1, p, 0, *slop, p, &mut out);
                    }
                }
                out
            }
            _ => unreachable!("only words and phrases are proximity operands"),
        }
    }

    fn matches(&self, doc: &[usize]) -> bool {
        match self {
            G::Word(_) | G::Phrase(..) => !self.spans(doc).is_empty(),
            G::Near(l, r, gap, ordered) => {
                let (ls, rs) = (l.spans(doc), r.spans(doc));
                let close =
                    |a: (usize, usize), b: (usize, usize)| b.0 > a.1 && b.0 - a.1 - 1 <= *gap as usize;
                ls.iter().any(|&a| rs.iter().any(|&b| close(a, b) || (!ordered && close(b, a))))
            }
            G::AtLeast(k, of) => of.iter().filter(|c| c.matches(doc)).count() >= *k,
            G::And(pos, neg) => pos.iter().all(|c| c.matches(doc)) && !neg.iter().any(|c| c.matches(doc)),
            G::Or(cs) => cs.iter().any(|c| c.matches(doc)),
        }
    }
}

fn gen_word(rng: &mut Rng) -> usize {
    rng.below(WORDS.len() as u64) as usize
}

/// A word or a phrase. Proximity operands get slop-free phrases: their
/// matches all have one length, so every match is minimal and "some match
/// of each side is close enough" means the same by brute force as under
/// minimal intervals.
fn gen_operand(rng: &mut Rng, allow_slop: bool) -> G {
    if rng.below(2) == 0 {
        return G::Word(gen_word(rng));
    }
    let n = 2 + rng.below(3) as usize;
    let mut slots = Vec::new();
    let mut off = 0;
    for _ in 0..n {
        let alts = if rng.below(4) == 0 { vec![gen_word(rng), gen_word(rng)] } else { vec![gen_word(rng)] };
        slots.push((alts, off));
        off += 1 + if rng.below(4) == 0 { 1 } else { 0 };
    }
    let slop = if allow_slop { rng.below(3) as u32 } else { 0 };
    G::Phrase(slots, slop)
}

fn gen(rng: &mut Rng, depth: u32) -> G {
    match if depth == 0 { rng.below(2) } else { rng.below(6) } {
        0 => G::Word(gen_word(rng)),
        1 => gen_operand(rng, true),
        2 => G::Near(
            Box::new(gen_operand(rng, false)),
            Box::new(gen_operand(rng, false)),
            rng.below(4) as u32,
            rng.below(2) == 0,
        ),
        3 => {
            let n = 3 + rng.below(2) as usize;
            G::AtLeast(2 + rng.below(n as u64 - 2) as usize, (0..n).map(|_| gen(rng, depth - 1)).collect())
        }
        4 => {
            let pos = (0..1 + rng.below(2)).map(|_| gen(rng, depth - 1)).collect();
            let neg = (0..rng.below(2)).map(|_| gen(rng, depth - 1)).collect();
            G::And(pos, neg)
        }
        _ => G::Or((0..2).map(|_| gen(rng, depth - 1)).collect()),
    }
}

#[test]
fn positional_queries_match_brute_force() {
    let mut rng = Rng(8);
    let docs: Vec<Vec<usize>> =
        (0..300).map(|_| (0..rng.below(14)).map(|_| gen_word(&mut rng)).collect()).collect();
    let texts: Vec<String> =
        docs.iter().map(|d| d.iter().map(|&w| WORDS[w]).collect::<Vec<_>>().join(" ")).collect();
    let tid = |i: usize| Tid::new(i as u32 / 20, (i % 20) as u16 + 1);
    let mut b = SegmentBuilder::open_ended(0);
    for (i, t) in texts.iter().enumerate() {
        b.add(tid(i), t);
    }
    let index = Index::from_segments(vec![b.finish()]);
    let mut a = Analyzer::new();
    let (mut checked, mut matched) = (0, 0);
    for _ in 0..600 {
        let g = gen(&mut rng, 2);
        let text = g.render();
        let q = Query::parse(&text, &mut a).unwrap_or_else(|e| panic!("{text}: {e}"));
        let plan = Plan::from_query(&q).unwrap_or_else(|e| panic!("{text}: {e}"));
        let candidates: Vec<Tid> = index.search_vec(&plan);
        for (i, doc) in docs.iter().enumerate() {
            let want = g.matches(doc);
            assert_eq!(q.matches_text(&texts[i], &mut a), want, "{text} on {:?}", texts[i]);
            let terms = a.unique_terms(&texts[i]);
            let in_plan = plan.matches(&SortedTerms(&terms));
            // The plan never misses a match, and is exact without a recheck.
            assert!(in_plan || !want, "plan misses: {text} on {:?}", texts[i]);
            if !plan.needs_recheck() {
                assert_eq!(in_plan, want, "exact plan: {text} on {:?}", texts[i]);
            }
            assert_eq!(candidates.binary_search(&tid(i)).is_ok(), in_plan, "index vs plan: {text}");
            checked += 1;
            matched += want as usize;
        }
    }
    // The generator must produce both outcomes often enough to mean something.
    assert!(matched > checked / 20 && matched < checked * 19 / 20, "{matched} of {checked}");
}
