//! Seeded synthetic query set, in the spirit of TIN's Stack Exchange set:
//! conjunctions, disjunctions, mixed boolean, and negations over real terms.

use rustc_hash::FxHashMap;
use tin_core::{Index, Plan};

/// Only terms in at least this many tuples are used, so queries do real work.
pub const MIN_DF: u64 = 1_000;
const TOP_N: usize = 500;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Conjunction,
    Disjunction,
    Mixed,
    Negation,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Conjunction, Kind::Disjunction, Kind::Mixed, Kind::Negation];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Conjunction => "Conjunction",
            Kind::Disjunction => "Disjunction",
            Kind::Mixed => "Mixed",
            Kind::Negation => "Negation",
        }
    }
}

pub struct Query {
    pub kind: Kind,
    pub text: String,
    pub plan: Plan,
}

pub struct QuerySet {
    queries: Vec<Query>,
    pub pool_size: usize,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

impl QuerySet {
    pub fn generate(index: &Index, per_kind: usize, seed: u64) -> Self {
        let mut df: FxHashMap<String, u64> = FxHashMap::default();
        for seg in index.segments() {
            seg.for_each_term(|term, tp| {
                if tp.doc_count() >= MIN_DF / 16 {
                    *df.entry(String::from_utf8_lossy(term).into_owned()).or_default() += tp.doc_count();
                }
            });
        }
        let mut pool: Vec<(String, u64)> = df.into_iter().filter(|&(_, d)| d >= MIN_DF).collect();
        pool.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        assert!(pool.len() >= 10, "corpus too small for the query generator");
        let top = TOP_N.min(pool.len());

        let mut rng = Rng(seed);
        let mut queries = Vec::with_capacity(per_kind * 4);
        for kind in Kind::ALL {
            for _ in 0..per_kind {
                let mut terms: Vec<String> = Vec::new();
                let n = match kind {
                    Kind::Conjunction | Kind::Disjunction => 2 + rng.below(2),
                    Kind::Mixed | Kind::Negation => 3,
                };
                while terms.len() < n {
                    let t = if rng.below(10) < 3 {
                        &pool[rng.below(top)].0
                    } else {
                        &pool[rng.below(pool.len())].0
                    };
                    if !terms.contains(t) {
                        terms.push(t.clone());
                    }
                }
                let term = |i: usize| Plan::Term(terms[i].clone());
                let (text, plan) = match kind {
                    Kind::Conjunction => (terms.join(" "), Plan::And((0..n).map(term).collect())),
                    Kind::Disjunction => (terms.join(" OR "), Plan::Or((0..n).map(term).collect())),
                    Kind::Mixed => (
                        format!("({} OR {}) {}", terms[0], terms[1], terms[2]),
                        Plan::And(vec![Plan::Or(vec![term(0), term(1)]), term(2)]),
                    ),
                    Kind::Negation => (
                        format!("{} {} -{}", terms[0], terms[1], terms[2]),
                        Plan::AndNot(Box::new(Plan::And(vec![term(0), term(1)])), Box::new(term(2))),
                    ),
                };
                queries.push(Query { kind, text, plan });
            }
        }
        QuerySet { queries, pool_size: pool.len() }
    }

    pub fn all(&self) -> &[Query] {
        &self.queries
    }

    pub fn of(&self, kind: Kind) -> impl Iterator<Item = &Query> {
        self.queries.iter().filter(move |q| q.kind == kind)
    }
}
