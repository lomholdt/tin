//! Query language and planning.
//!
//! ```text
//! denim jeans            both terms (implicit AND)
//! denim AND jeans        same
//! denim OR chino         either
//! denim -stretch         denim but not stretch   (also: NOT stretch)
//! (denim OR chino) blue  grouping
//! msku12*                a term starting with "msku12"
//! *1234565*              a term containing "1234565" (also *1234565)
//! msku1243565~           a term within 1 edit (~2: two edits); a swap of
//!                        neighbouring characters counts as one edit
//! ```
//!
//! `AND` / `OR` / `NOT` are keywords only in upper case. Words go through the
//! same [`Analyzer`] as documents; a word the analyzer splits into several
//! terms (`e-mail`) becomes an AND of them until phrases land in Phase 5.
//! `"quoted phrases"` are rejected for now rather than silently degraded.
//! Patterns apply to a single term and are matched against whole terms.

use std::collections::HashSet;
use std::fmt;

use crate::pattern::osa_within;
use crate::tokenize::Analyzer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Term(String),
    Prefix(String),
    Fragment(String),
    Fuzzy(String, u8),
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// Nothing searchable in the query.
    Empty,
    UnbalancedParens,
    /// Phrase queries arrive in Phase 5.
    PhraseUnsupported,
    /// A negation with nothing positive to subtract from (`-foo`, `a OR -b`).
    UnboundedNegation,
    /// A `*` / `~` pattern that isn't exactly one term.
    BadPattern(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            QueryError::Empty => "query has no searchable terms",
            QueryError::UnbalancedParens => "unbalanced parentheses",
            QueryError::PhraseUnsupported => "phrase queries are not supported yet",
            QueryError::UnboundedNegation => "negation needs a positive term beside it",
            QueryError::BadPattern(w) => {
                return write!(f, "pattern {w:?} must be one term: word*, *fragment*, or word~ / word~2");
            }
        })
    }
}

impl std::error::Error for QueryError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LParen,
    RParen,
    Neg,
    And,
    Or,
    Word(String),
}

fn lex(input: &str) -> Result<Vec<Tok>, QueryError> {
    let mut toks = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, toks: &mut Vec<Tok>| {
        if !word.is_empty() {
            toks.push(match word.as_str() {
                "AND" => Tok::And,
                "OR" => Tok::Or,
                "NOT" => Tok::Neg,
                _ => Tok::Word(std::mem::take(word)),
            });
            word.clear();
        }
    };
    for c in input.chars() {
        match c {
            '"' => return Err(QueryError::PhraseUnsupported),
            '(' | ')' => {
                flush(&mut word, &mut toks);
                toks.push(if c == '(' { Tok::LParen } else { Tok::RParen });
            }
            '-' if word.is_empty() => toks.push(Tok::Neg),
            c if c.is_whitespace() => flush(&mut word, &mut toks),
            c => word.push(c),
        }
    }
    flush(&mut word, &mut toks);
    Ok(toks)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    analyzer: &'a mut Analyzer,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn or_expr(&mut self) -> Result<Option<Query>, QueryError> {
        let mut parts = Vec::new();
        parts.extend(self.and_expr()?);
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            parts.extend(self.and_expr()?);
        }
        Ok(collapse(parts, Query::Or))
    }

    fn and_expr(&mut self) -> Result<Option<Query>, QueryError> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::RParen) | Some(Tok::Or) => break,
                Some(Tok::And) => self.pos += 1,
                _ => parts.extend(self.unary()?),
            }
        }
        Ok(collapse(parts, Query::And))
    }

    fn unary(&mut self) -> Result<Option<Query>, QueryError> {
        let tok = self.toks[self.pos].clone();
        self.pos += 1;
        match tok {
            Tok::Neg => match self.peek() {
                // A stray `-` / `NOT` with nothing to negate is ignored.
                None | Some(Tok::RParen | Tok::Or | Tok::And) => Ok(None),
                _ => Ok(self.unary()?.map(|q| Query::Not(Box::new(q)))),
            },
            Tok::LParen => {
                let q = self.or_expr()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(QueryError::UnbalancedParens);
                }
                self.pos += 1;
                Ok(q)
            }
            Tok::RParen => Err(QueryError::UnbalancedParens),
            Tok::Word(w) => self.word(&w),
            Tok::And | Tok::Or => unreachable!("handled by callers"),
        }
    }
}

impl Parser<'_> {
    /// A plain word (analyzed, possibly into several terms) or a pattern.
    fn word(&mut self, w: &str) -> Result<Option<Query>, QueryError> {
        let lead = w.starts_with('*');
        let core = w.trim_start_matches('*');
        let trail = core.ends_with('*');
        let core = core.trim_end_matches('*');
        let (core, fuzz) = match core.rsplit_once('~') {
            Some((c, "")) => (c, Some(1)),
            Some((c, "1")) => (c, Some(1)),
            Some((c, "2")) => (c, Some(2)),
            _ => (core, None),
        };
        if !lead && !trail && fuzz.is_none() {
            let terms = self.analyzer.terms(w);
            return Ok(collapse(terms.into_iter().map(Query::Term).collect(), Query::And));
        }
        let mut terms = self.analyzer.terms(core);
        if terms.len() != 1 || (fuzz.is_some() && (lead || trail)) {
            return Err(QueryError::BadPattern(w.to_owned()));
        }
        let t = terms.pop().unwrap();
        Ok(Some(match (lead, trail, fuzz) {
            (_, _, Some(k)) => Query::Fuzzy(t, k),
            (true, _, None) => Query::Fragment(t),
            (false, true, None) => Query::Prefix(t),
            (false, false, None) => unreachable!(),
        }))
    }
}

fn collapse(mut parts: Vec<Query>, wrap: fn(Vec<Query>) -> Query) -> Option<Query> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(wrap(parts)),
    }
}

impl Query {
    pub fn parse(input: &str, analyzer: &mut Analyzer) -> Result<Query, QueryError> {
        let mut p = Parser { toks: lex(input)?, pos: 0, analyzer };
        let q = p.or_expr()?;
        if p.pos != p.toks.len() {
            return Err(QueryError::UnbalancedParens);
        }
        q.ok_or(QueryError::Empty)
    }
}

/// A validated, normalized query ready to run against any segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Term(String),
    /// A term starting with this.
    Prefix(String),
    /// A term containing this.
    Fragment(String),
    /// A term within this many OSA edits.
    Fuzzy(String, u8),
    And(Vec<Plan>),
    Or(Vec<Plan>),
    /// `positive AND NOT negative`.
    AndNot(Box<Plan>, Box<Plan>),
}

impl Plan {
    pub fn parse(input: &str, analyzer: &mut Analyzer) -> Result<Plan, QueryError> {
        Plan::from_query(&Query::parse(input, analyzer)?)
    }

    pub fn from_query(q: &Query) -> Result<Plan, QueryError> {
        match q {
            Query::Term(t) => Ok(Plan::Term(t.clone())),
            Query::Prefix(t) => Ok(Plan::Prefix(t.clone())),
            Query::Fragment(t) => Ok(Plan::Fragment(t.clone())),
            Query::Fuzzy(t, k) => Ok(Plan::Fuzzy(t.clone(), *k)),
            Query::Not(_) => Err(QueryError::UnboundedNegation),
            Query::Or(cs) => Ok(Plan::Or(cs.iter().map(Plan::from_query).collect::<Result<_, _>>()?)),
            Query::And(cs) => {
                let mut pos = Vec::new();
                let mut neg = Vec::new();
                for c in cs {
                    match c {
                        Query::Not(inner) => neg.push(Plan::from_query(inner)?),
                        other => pos.push(Plan::from_query(other)?),
                    }
                }
                let pos = match pos.len() {
                    0 => return Err(QueryError::UnboundedNegation),
                    1 => pos.pop().unwrap(),
                    _ => Plan::And(pos),
                };
                Ok(match neg.len() {
                    0 => pos,
                    1 => Plan::AndNot(Box::new(pos), Box::new(neg.pop().unwrap())),
                    _ => Plan::AndNot(Box::new(pos), Box::new(Plan::Or(neg))),
                })
            }
        }
    }
}

/// One document's distinct terms, for evaluating a [`Plan`] without an
/// index (sequential scans, rechecks, pending-list records).
pub trait TermSet {
    fn contains_term(&self, term: &str) -> bool;
    /// Whether any term satisfies `f`.
    fn any_term(&self, f: &mut dyn FnMut(&str) -> bool) -> bool;
}

impl TermSet for HashSet<String> {
    fn contains_term(&self, term: &str) -> bool {
        self.contains(term)
    }
    fn any_term(&self, f: &mut dyn FnMut(&str) -> bool) -> bool {
        self.iter().any(|t| f(t))
    }
}

/// Distinct terms in sorted order.
pub struct SortedTerms<'a>(pub &'a [String]);

impl TermSet for SortedTerms<'_> {
    fn contains_term(&self, term: &str) -> bool {
        self.0.binary_search_by(|x| x.as_str().cmp(term)).is_ok()
    }
    fn any_term(&self, f: &mut dyn FnMut(&str) -> bool) -> bool {
        self.0.iter().any(|t| f(t))
    }
}

impl Plan {
    /// Evaluate against one document's terms.
    pub fn matches(&self, doc: &dyn TermSet) -> bool {
        match self {
            Plan::Term(t) => doc.contains_term(t),
            Plan::Prefix(p) => doc.any_term(&mut |t| t.starts_with(p.as_str())),
            Plan::Fragment(f) => doc.any_term(&mut |t| t.contains(f.as_str())),
            Plan::Fuzzy(q, k) => doc.any_term(&mut |t| osa_within(t, q, *k)),
            Plan::And(cs) => cs.iter().all(|c| c.matches(doc)),
            Plan::Or(cs) => cs.iter().any(|c| c.matches(doc)),
            Plan::AndNot(p, n) => p.matches(doc) && !n.matches(doc),
        }
    }

    /// Analyze `text` and evaluate against it: the non-index path (sequential
    /// scans, rechecks) that must agree with index results.
    pub fn matches_text(&self, text: &str, analyzer: &mut Analyzer) -> bool {
        let mut terms = HashSet::new();
        analyzer.for_each_term(text, |t, _| {
            if !terms.contains(t) {
                terms.insert(t.to_owned());
            }
        });
        self.matches(&terms)
    }

    /// Whether the index may return tuples that don't match (fragments
    /// resolved through grams), so callers must recheck candidates.
    pub fn needs_recheck(&self) -> bool {
        match self {
            Plan::Fragment(_) => true,
            Plan::Term(_) | Plan::Prefix(_) | Plan::Fuzzy(..) => false,
            Plan::And(cs) | Plan::Or(cs) => cs.iter().any(Plan::needs_recheck),
            // Fragments under a negation are always resolved exactly.
            Plan::AndNot(p, _) => p.needs_recheck(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(s: &str) -> Result<Plan, QueryError> {
        Plan::parse(s, &mut Analyzer::new())
    }
    fn t(s: &str) -> Plan {
        Plan::Term(s.into())
    }

    #[test]
    fn precedence_and_grouping() {
        assert_eq!(plan("a b OR c").unwrap(), Plan::Or(vec![Plan::And(vec![t("a"), t("b")]), t("c")]));
        assert_eq!(plan("a (b OR c)").unwrap(), Plan::And(vec![t("a"), Plan::Or(vec![t("b"), t("c")])]));
        assert_eq!(plan("a AND b").unwrap(), plan("a b").unwrap());
        assert_eq!(plan("Café").unwrap(), t("cafe"));
    }

    #[test]
    fn negation() {
        assert_eq!(plan("a -b").unwrap(), Plan::AndNot(Box::new(t("a")), Box::new(t("b"))));
        assert_eq!(
            plan("a NOT b -c").unwrap(),
            Plan::AndNot(Box::new(t("a")), Box::new(Plan::Or(vec![t("b"), t("c")])))
        );
        assert_eq!(plan("-a"), Err(QueryError::UnboundedNegation));
        assert_eq!(plan("a OR -b"), Err(QueryError::UnboundedNegation));
        // A hyphen inside a word is not negation.
        assert_eq!(plan("e-mail").unwrap(), Plan::And(vec![t("e"), t("mail")]));
    }

    #[test]
    fn matches_text() {
        let mut a = Analyzer::new();
        let p = plan("(denim OR chino) -stretch").unwrap();
        assert!(p.matches_text("Raw DENIM jacket", &mut a));
        assert!(!p.matches_text("stretch denim", &mut a));
        assert!(!p.matches_text("wool coat", &mut a));
    }

    #[test]
    fn patterns() {
        assert_eq!(plan("MSKU12*").unwrap(), Plan::Prefix("msku12".into()));
        assert_eq!(plan("*1234565*").unwrap(), Plan::Fragment("1234565".into()));
        assert_eq!(plan("*4565").unwrap(), Plan::Fragment("4565".into()));
        assert_eq!(plan("msku1243565~").unwrap(), Plan::Fuzzy("msku1243565".into(), 1));
        assert_eq!(plan("msku1243565~2").unwrap(), Plan::Fuzzy("msku1243565".into(), 2));
        assert_eq!(
            plan("maeu* -*999*").unwrap(),
            Plan::AndNot(Box::new(Plan::Prefix("maeu".into())), Box::new(Plan::Fragment("999".into())))
        );
        assert!(matches!(plan("e-mail*"), Err(QueryError::BadPattern(_))));
        assert!(matches!(plan("*abc~"), Err(QueryError::BadPattern(_))));
        let mut a = Analyzer::new();
        let doc = "MSKU6018200 234567890 MAEU123456789";
        for (q, want) in [
            ("msku60*", true),
            ("msku7*", false),
            ("*18200*", true),
            ("*4567*", true),
            ("*99*", false),
            ("msku6012800~", true), // swap 8<->2
            ("msku6012801~", false),
            ("msku6012801~2", true),
            ("maeu* -*999*", true),
            ("maeu* -*4567*", false),
        ] {
            assert_eq!(plan(q).unwrap().matches_text(doc, &mut a), want, "{q}");
        }
        assert!(plan("a *bc* d").unwrap().needs_recheck());
        assert!(!plan("a -*bc*").unwrap().needs_recheck());
        assert!(!plan("abc* def~").unwrap().needs_recheck());
    }

    #[test]
    fn errors() {
        assert_eq!(plan("  ... "), Err(QueryError::Empty));
        assert_eq!(plan("(a b"), Err(QueryError::UnbalancedParens));
        assert_eq!(plan("a b)"), Err(QueryError::UnbalancedParens));
        assert_eq!(plan("a -").unwrap(), t("a"));
        assert_eq!(plan("a NOT OR b").unwrap(), Plan::Or(vec![t("a"), t("b")]));
        assert_eq!(plan("\"a b\""), Err(QueryError::PhraseUnsupported));
    }
}
