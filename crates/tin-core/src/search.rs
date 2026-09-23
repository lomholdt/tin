//! Search-box queries: one typed string, matched the way a search box should
//! and ranked by *how* it matched.
//!
//! Every query term is compared with a document's terms at five **tiers**:
//!
//! | tier | the document has a term that…        | applies to query terms of |
//! |-----:|---------------------------------------|---------------------------|
//! | 0    | equals it                             | any length                |
//! | 1    | starts with it                        | any length                |
//! | 2    | contains it                           | [`FRAGMENT_MIN`]+ chars   |
//! | 3    | is within 1 edit (OSA)                | [`TYPO1_MIN`]+ chars      |
//! | 4    | is within 2 edits                     | [`TYPO2_MIN`]+ chars      |
//!
//! A document's tier for the whole query is the worst over query terms of
//! each term's best tier (all terms must match, each as well as it can);
//! no tier means no match. [`SearchBox::plan`] gives the index plan for
//! "tier ≤ L", so an index can produce matches tier by tier and stop early.

use fst::Automaton;

use crate::pattern::OsaDfa;
use crate::query::Plan;
use crate::tokenize::Analyzer;

pub const FRAGMENT_MIN: usize = 3;
pub const TYPO1_MIN: usize = 4;
pub const TYPO2_MIN: usize = 7;
/// The worst tier.
pub const MAX_TIER: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchBox {
    terms: Vec<String>,
    /// Typo tiers used: 0 (none), 1 (tier 3), or 2 (tiers 3 and 4).
    max_typos: u8,
}

impl SearchBox {
    pub fn parse(input: &str, analyzer: &mut Analyzer) -> SearchBox {
        SearchBox { terms: analyzer.unique_terms(input), max_typos: 2 }
    }

    /// Allow at most `n` typos (0–2) per term, like Typesense's `num_typos`:
    /// fewer typo tiers, so a scan that must fill k rows stops sooner.
    pub fn with_max_typos(mut self, n: u8) -> SearchBox {
        self.max_typos = n.min(2);
        self
    }

    fn allows(&self, q: &str, tier: u8) -> bool {
        term_has_tier(q, tier) && (tier < 3 || tier - 2 <= self.max_typos)
    }

    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Whether some query term gains a way to match at `tier` (otherwise
    /// tier `tier` holds nothing new).
    pub fn has_tier(&self, tier: u8) -> bool {
        match tier {
            0 => !self.terms.is_empty(),
            _ => self.terms.iter().any(|t| self.allows(t, tier)),
        }
    }

    /// Documents at tier ≤ `tier`. Plans with fragments give candidates
    /// ([`Plan::needs_recheck`]).
    pub fn plan(&self, tier: u8) -> Plan {
        let per_term: Vec<Plan> = self
            .terms
            .iter()
            .map(|t| {
                let mut alts = vec![Plan::Term(t.clone())];
                for l in 1..=tier {
                    if self.allows(t, l) {
                        alts.push(match l {
                            1 => Plan::Prefix(t.clone()),
                            2 => Plan::Fragment(t.clone()),
                            3 => Plan::Fuzzy(t.clone(), 1),
                            _ => Plan::Fuzzy(t.clone(), 2),
                        });
                    }
                }
                if alts.len() == 1 {
                    alts.pop().unwrap()
                } else {
                    Plan::Or(alts)
                }
            })
            .collect();
        match per_term.len() {
            0 => Plan::Or(Vec::new()),
            1 => per_term.into_iter().next().unwrap(),
            _ => Plan::And(per_term),
        }
    }

    /// The tier of a document with these (analyzed) terms, if it matches.
    /// (Compiles a [`Matcher`]; keep one to check many documents.)
    pub fn tier_of_terms<S: AsRef<str>>(&self, doc: &[S]) -> Option<u8> {
        self.matcher().tier_of_terms(doc)
    }

    pub fn tier_of_text(&self, text: &str, analyzer: &mut Analyzer) -> Option<u8> {
        self.tier_of_terms(&analyzer.unique_terms(text))
    }

    /// This query compiled for checking many documents.
    pub fn matcher(&self) -> Matcher {
        let terms = self
            .terms
            .iter()
            .map(|q| TermMatcher {
                q: q.clone(),
                fragment: self.allows(q, 2),
                typo1: self.allows(q, 3).then(|| OsaDfa::new(q.as_bytes(), 1)),
                typo2: self.allows(q, 4).then(|| OsaDfa::new(q.as_bytes(), 2)),
            })
            .collect();
        Matcher { terms }
    }
}

/// A [`SearchBox`] compiled for checking many documents (pending records,
/// rechecks): typo automata built once, cheap tests before them. Checking a
/// document term allocates nothing.
pub struct Matcher {
    terms: Vec<TermMatcher>,
}

struct TermMatcher {
    q: String,
    fragment: bool,
    typo1: Option<OsaDfa>,
    typo2: Option<OsaDfa>,
}

impl Matcher {
    pub fn tier_of_terms<S: AsRef<str>>(&self, doc: &[S]) -> Option<u8> {
        if self.terms.is_empty() {
            return None;
        }
        let mut worst = 0;
        for t in &self.terms {
            let mut best = None;
            for d in doc {
                if let Some(x) = t.tier(d.as_ref()) {
                    if best.is_none_or(|b| x < b) {
                        best = Some(x);
                        if x == 0 {
                            break;
                        }
                    }
                }
            }
            worst = worst.max(best?);
        }
        Some(worst)
    }

    pub fn tier_of_text(&self, text: &str, analyzer: &mut Analyzer) -> Option<u8> {
        self.tier_of_terms(&analyzer.unique_terms(text))
    }
}

impl TermMatcher {
    /// How well document term `d` matches this query term.
    fn tier(&self, d: &str) -> Option<u8> {
        let q = self.q.as_str();
        if d == q {
            return Some(0);
        }
        if d.starts_with(q) {
            return Some(1);
        }
        if self.fragment && d.contains(q) {
            return Some(2);
        }
        let diff = d.len().abs_diff(q.len());
        if diff <= 1 && self.typo1.as_ref().is_some_and(|a| accepts(a, d)) {
            return Some(3);
        }
        if diff <= 2 && self.typo2.as_ref().is_some_and(|a| accepts(a, d)) {
            return Some(4);
        }
        None
    }
}

fn accepts(dfa: &OsaDfa, d: &str) -> bool {
    let mut s = dfa.start();
    for &c in d.as_bytes() {
        if !dfa.can_match(&s) {
            return false;
        }
        s = dfa.accept(&s, c);
    }
    dfa.is_match(&s)
}

fn term_has_tier(q: &str, tier: u8) -> bool {
    let n = q.chars().count();
    match tier {
        0 | 1 => true,
        2 => n >= FRAGMENT_MIN,
        3 => n >= TYPO1_MIN,
        4 => n >= TYPO2_MIN,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::SortedTerms;

    fn sb(q: &str) -> SearchBox {
        SearchBox::parse(q, &mut Analyzer::new())
    }

    #[test]
    fn tiers() {
        let q = sb("MSKU6018200");
        let t = |doc: &str| q.tier_of_text(doc, &mut Analyzer::new());
        assert_eq!(t("msku6018200 123"), Some(0));
        assert_eq!(t("msku60182001"), Some(1));
        assert_eq!(t("xmsku6018200"), Some(2));
        assert_eq!(t("msku6012800"), Some(3)); // swap
        assert_eq!(t("msku6012801"), Some(4));
        assert_eq!(t("cmau1234567"), None);
        // Every term must match; the worst one decides.
        let q2 = sb("msku60 810698640");
        assert_eq!(q2.tier_of_text("msku6018200 810698640", &mut Analyzer::new()), Some(1));
        assert_eq!(q2.tier_of_text("msku6018200", &mut Analyzer::new()), None);
        // Short terms don't get fuzzy tiers.
        assert!(!sb("ab").has_tier(2) && sb("abc").has_tier(2) && !sb("abc").has_tier(3));
        assert_eq!(sb("").tier_of_text("x", &mut Analyzer::new()), None);
        // A typo budget drops the typo tiers beyond it.
        let q1 = sb("MSKU6018200").with_max_typos(1);
        assert_eq!(q1.tier_of_text("msku6012800", &mut Analyzer::new()), Some(3));
        assert_eq!(q1.tier_of_text("msku6012801", &mut Analyzer::new()), None);
        assert!(q1.has_tier(3) && !q1.has_tier(4) && !sb("MSKU6018200").with_max_typos(0).has_tier(3));
    }

    #[test]
    fn plans_agree_with_tiers() {
        let docs =
            ["msku6018200 810698640", "msku60182001", "xmsku6018200", "msku6012800", "msku6012801", "a b"];
        for (q, typos) in [
            ("msku6018200", 2),
            ("msku60", 2),
            ("018200", 2),
            ("msku6018200 81069864", 2),
            ("b", 2),
            ("msku6018200", 1),
            ("msku6018200", 0),
        ] {
            let q = sb(q).with_max_typos(typos);
            for d in docs {
                let terms = Analyzer::new().unique_terms(d);
                let tier = q.tier_of_terms(&terms);
                for l in 0..=MAX_TIER {
                    let in_plan = q.plan(l).matches(&SortedTerms(&terms));
                    assert_eq!(in_plan, tier.is_some_and(|t| t <= l), "{q:?} {d} tier ≤ {l}");
                }
            }
        }
    }
}
