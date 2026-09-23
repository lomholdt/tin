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

use crate::pattern::osa_within;
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
    pub fn tier_of_terms<S: AsRef<str>>(&self, doc: &[S]) -> Option<u8> {
        if self.terms.is_empty() {
            return None;
        }
        let mut worst = 0;
        for q in &self.terms {
            let best = doc.iter().filter_map(|d| self.term_tier(q, d.as_ref())).min()?;
            worst = worst.max(best);
        }
        Some(worst)
    }

    pub fn tier_of_text(&self, text: &str, analyzer: &mut Analyzer) -> Option<u8> {
        self.tier_of_terms(&analyzer.unique_terms(text))
    }

    /// How well document term `d` matches query term `q`.
    fn term_tier(&self, q: &str, d: &str) -> Option<u8> {
        if d == q {
            Some(0)
        } else if d.starts_with(q) {
            Some(1)
        } else if self.allows(q, 2) && d.contains(q) {
            Some(2)
        } else if self.allows(q, 3) && osa_within(d, q, 1) {
            Some(3)
        } else if self.allows(q, 4) && osa_within(d, q, 2) {
            Some(4)
        } else {
            None
        }
    }
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
