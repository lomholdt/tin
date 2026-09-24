//! Relevance scores: BM25 over a query's positive terms.
//!
//! The index keeps only which rows hold a term, not how often, so scores
//! are computed like TIN's: from the row's own text (term frequencies and
//! length, via [`DocWords`]) and collection statistics the index does have
//! (row count, each term's document frequency, total postings).
//!
//! * **Weights**: every leaf outside a negation carries the product of the
//!   boosts above it; a document term matched by several leaves gets the
//!   sum of their weights (so `"craft beer"^3 OR craft` weighs `craft` 4).
//! * **Patterns** (`brew*`, `*craft*`, `beer~`) score each document term
//!   they match as a term of its own.
//! * **Length**: a document's length is its number of distinct terms, the
//!   quantity the index's postings add up to, so the average length is
//!   exact without storing lengths (4-grams excluded).
//! * **Non-matching rows** are scored like any other; filter with `==>`.

use rustc_hash::FxHashMap;

use crate::query::Query;
use crate::span::{DocWords, Wanted};

/// BM25 term-frequency saturation.
pub const K1: f64 = 1.2;
/// BM25 length normalization.
pub const B: f64 = 0.75;

/// What a score needs from the collection.
pub trait CollectionStats {
    /// Rows in the collection.
    fn docs(&self) -> f64;
    /// Average distinct terms per row.
    fn avg_len(&self) -> f64;
    /// Rows holding `term`.
    fn doc_freq(&self, term: &str) -> u64;
}

/// One document term's part of a score.
#[derive(Clone, Debug, PartialEq)]
pub struct TermScore {
    pub term: String,
    /// Sum of the boosts of the query leaves matching it.
    pub weight: f64,
    pub tf: u32,
    pub df: u64,
    pub idf: f64,
    pub score: f64,
}

/// A score and how it came about.
#[derive(Clone, Debug, PartialEq)]
pub struct Explanation {
    pub score: f64,
    /// Distinct terms in the document.
    pub doc_len: usize,
    pub avg_len: f64,
    pub docs: f64,
    /// By descending contribution.
    pub terms: Vec<TermScore>,
}

/// A query's positive leaves (terms and patterns) and their weights: the
/// part of a query scores depend on. Build once per query.
pub struct Scorer {
    leaves: Vec<(Query, f64)>,
    wanted: Wanted,
}

impl Scorer {
    pub fn new(q: &Query) -> Scorer {
        let mut leaves: Vec<(Query, f64)> = Vec::new();
        collect(q, 1.0, &mut leaves);
        leaves.retain(|(_, w)| *w > 0.0);
        Scorer { leaves, wanted: Wanted::counting(q) }
    }

    /// What to read from a document before scoring it:
    /// `DocWords::with(text, analyzer, scorer.wanted())`.
    pub fn wanted(&self) -> &Wanted {
        &self.wanted
    }

    pub fn score(&self, doc: &DocWords, stats: &dyn CollectionStats) -> f64 {
        self.explain(doc, stats).score
    }

    pub fn explain(&self, doc: &DocWords, stats: &dyn CollectionStats) -> Explanation {
        let mut weights: FxHashMap<&str, (f64, u32)> = FxHashMap::default();
        for (leaf, w) in &self.leaves {
            for (term, positions) in doc.leaf_terms(leaf) {
                weights.entry(term).or_insert((0.0, positions.len() as u32)).0 += w;
            }
        }
        let n = stats.docs().max(1.0);
        let avg_len = stats.avg_len().max(1.0);
        let doc_len = doc.distinct_terms();
        let norm = K1 * (1.0 - B + B * doc_len as f64 / avg_len);
        let mut terms: Vec<TermScore> = weights
            .into_iter()
            .map(|(term, (weight, tf))| {
                let df = stats.doc_freq(term).max(1);
                let idf = (1.0 + (n - df as f64 + 0.5).max(0.0) / (df as f64 + 0.5)).ln();
                let tf_part = tf as f64 * (K1 + 1.0) / (tf as f64 + norm);
                TermScore { term: term.to_owned(), weight, tf, df, idf, score: weight * idf * tf_part }
            })
            .collect();
        terms.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.term.cmp(&b.term)));
        Explanation { score: terms.iter().map(|t| t.score).sum(), doc_len, avg_len, docs: n, terms }
    }
}

/// Leaves under `q` with `weight` times the boosts on the way, merged by
/// leaf (weights add up).
fn collect(q: &Query, weight: f64, out: &mut Vec<(Query, f64)>) {
    match q {
        Query::Term(_) | Query::Prefix(_) | Query::Fragment(_) | Query::Fuzzy(..) => {
            match out.iter_mut().find(|(l, _)| l == q) {
                Some((_, w)) => *w += weight,
                None => out.push((q.clone(), weight)),
            }
        }
        Query::Not(_) => {}
        Query::Boost(q, b) => collect(q, weight * *b as f64, out),
        Query::And(cs) | Query::Or(cs) | Query::AtLeast { of: cs, .. } => {
            cs.iter().for_each(|c| collect(c, weight, out))
        }
        Query::Phrase { slots, .. } => {
            slots.iter().flat_map(|s| &s.alts).for_each(|a| collect(a, weight, out))
        }
        Query::Near { left, right, .. } => {
            collect(left, weight, out);
            collect(right, weight, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::Analyzer;

    struct Fixed(FxHashMap<&'static str, u64>);
    impl CollectionStats for Fixed {
        fn docs(&self) -> f64 {
            1000.0
        }
        fn avg_len(&self) -> f64 {
            10.0
        }
        fn doc_freq(&self, term: &str) -> u64 {
            *self.0.get(term).unwrap_or(&1)
        }
    }

    fn score(q: &str, doc: &str) -> Explanation {
        let mut a = Analyzer::new();
        let stats = Fixed([("common", 900), ("rare", 2), ("beer", 50), ("craft", 50)].into_iter().collect());
        let scorer = Scorer::new(&Query::parse(q, &mut a).unwrap());
        let e = scorer.explain(&DocWords::with(doc, &mut a, scorer.wanted()), &stats);
        // Reading only the query's terms changes nothing.
        assert_eq!(e, scorer.explain(&DocWords::new(doc, &mut a), &stats));
        e
    }

    #[test]
    fn bm25() {
        // Rare terms outweigh common ones; repeats help, with saturation.
        assert!(score("rare", "rare x").score > score("common", "common x").score);
        let (one, two, ten) =
            (score("beer", "beer x"), score("beer", "beer beer x"), score("beer", &"beer ".repeat(10)));
        assert!(two.score > one.score && ten.score < 2.2 * one.score * (K1 + 1.0));
        // Longer documents score lower for the same counts.
        assert!(score("beer", "beer a b c d e f g h i j k l").score < one.score);
        // By hand: n = 1000, df = 50, tf = 1, two distinct terms, average 10.
        let idf = (1.0f64 + 950.5 / 50.5).ln();
        let want = idf * 2.2 / (1.0 + K1 * (1.0 - B + B * 0.2));
        assert!((one.score - want).abs() < 1e-9, "{} vs {want}", one.score);
        // Terms of a negation don't count; unmatched terms contribute nothing.
        assert_eq!(score("beer AND NOT rare", "beer x").score, one.score);
        assert_eq!(score("beer wine", "beer x").score, one.score);
    }

    #[test]
    fn boosts_and_patterns() {
        let one = score("beer", "beer x").score;
        assert!((score("beer^2", "beer x").score - 2.0 * one).abs() < 1e-9);
        assert_eq!(score("beer^0", "beer x").score, 0.0);
        // Weights of the same term add up across branches.
        let e = score("\"craft beer\"^3 OR (craft NEAR/5 beer)", "craft beer");
        assert!(e.terms.iter().all(|t| t.weight == 4.0), "{e:?}");
        // A pattern scores each term it matches.
        let e = score("bee*", "beer beet x");
        // (beet is rarer than beer here, so it comes first.)
        assert_eq!(e.terms.iter().map(|t| t.term.as_str()).collect::<Vec<_>>(), ["beet", "beer"]);
    }
}
