//! Query language and planning.
//!
//! Syntax (Phase 0):
//!
//! ```text
//! denim jeans            both terms (implicit AND)
//! denim AND jeans        same
//! denim OR chino         either
//! denim -stretch         denim but not stretch   (also: NOT stretch)
//! (denim OR chino) blue  grouping
//! ```
//!
//! `AND` / `OR` / `NOT` are keywords only in upper case. Words go through the
//! same [`Analyzer`] as documents; a word the analyzer splits into several
//! terms (`e-mail`) becomes an AND of them until phrases land in Phase 5.
//! `"quoted phrases"` are rejected for now rather than silently degraded.

use crate::tokenize::Analyzer;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Term(String),
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
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            QueryError::Empty => "query has no searchable terms",
            QueryError::UnbalancedParens => "unbalanced parentheses",
            QueryError::PhraseUnsupported => "phrase queries are not supported yet",
            QueryError::UnboundedNegation => "negation needs a positive term beside it",
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
            Tok::Word(w) => {
                let terms = self.analyzer.terms(&w);
                Ok(collapse(terms.into_iter().map(Query::Term).collect(), Query::And))
            }
            Tok::And | Tok::Or => unreachable!("handled by callers"),
        }
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

impl Plan {
    /// Evaluate against one document's term set. `has(term)` says whether
    /// the document contains `term`.
    pub fn matches(&self, has: &impl Fn(&str) -> bool) -> bool {
        match self {
            Plan::Term(t) => has(t),
            Plan::And(cs) => cs.iter().all(|c| c.matches(has)),
            Plan::Or(cs) => cs.iter().any(|c| c.matches(has)),
            Plan::AndNot(p, n) => p.matches(has) && !n.matches(has),
        }
    }

    /// Analyze `text` and evaluate against it: the non-index path (sequential
    /// scans, rechecks) that must agree with index results.
    pub fn matches_text(&self, text: &str, analyzer: &mut Analyzer) -> bool {
        let mut terms = std::collections::HashSet::new();
        analyzer.for_each_term(text, |t, _| {
            if !terms.contains(t) {
                terms.insert(t.to_owned());
            }
        });
        self.matches(&|t| terms.contains(t))
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
    fn errors() {
        assert_eq!(plan("  ... "), Err(QueryError::Empty));
        assert_eq!(plan("(a b"), Err(QueryError::UnbalancedParens));
        assert_eq!(plan("a b)"), Err(QueryError::UnbalancedParens));
        assert_eq!(plan("a -").unwrap(), t("a"));
        assert_eq!(plan("a NOT OR b").unwrap(), Plan::Or(vec![t("a"), t("b")]));
        assert_eq!(plan("\"a b\""), Err(QueryError::PhraseUnsupported));
    }
}
