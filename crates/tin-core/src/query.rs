//! Query language (TINQL-style) and planning.
//!
//! ```text
//! denim jeans              both terms (implicit AND; also denim AND jeans)
//! denim OR chino           either
//! denim AND NOT stretch    denim but not stretch (also: denim -stretch,
//!                          denim NOT stretch)
//! (denim OR chino) blue    grouping
//! [denim chino corduroy]   alternatives: any of them (commas optional)
//! "raw denim jacket"       a phrase: consecutive words, in order
//! "big _ wolf"             `_` skips exactly one word
//! "[big large] bad wolf"   alternatives for one position
//! "big bad wolf"~2         up to 2 extra words anywhere in the phrase
//! craft THEN/3 beer        beer after craft, at most 3 words between
//! craft NEAR/3 beer        the same in either order
//! AT LEAST 2 OF [a b c]    at least 2 of the items (also 50%; ALL OF [...])
//! beer^2 "craft beer"^0.5  boost: scales the item's weight in scores
//! msku12*                  a term starting with "msku12"
//! *1234565*                a term containing "1234565" (also *1234565)
//! msku1243565~             a term within 1 edit (~2: two edits); a swap of
//!                          neighbouring characters counts as one edit
//! ```
//!
//! Precedence, loosest first: `OR`, `AND` (explicit or implicit) and
//! negation, `THEN/N` / `NEAR/N` (left to right), boost `^`, then items.
//! Keywords are keywords only in upper case. Words go through the same
//! [`Analyzer`] as documents; a word it splits into several terms (`e-mail`)
//! is a phrase of them. Patterns apply to a single term.
//!
//! The index answers term-level questions only, so phrases, proximity and
//! `AT LEAST` are planned as a superset (the terms they need) and rechecked
//! against the row's word positions ([`Query::matches_text`]); see
//! [`crate::span`].
//!
//! Differences from TINQL: a leading `-` negates (TINQL keeps hyphens in the
//! term); `~N` on a word is an OSA edit distance with no fixed prefix. Not
//! supported yet: `?` wildcards, `TO` ranges, `MATCHES`, `WITHIN`,
//! positional filters and span relations.

use std::collections::HashSet;
use std::fmt;

use crate::pattern::osa_within;
use crate::tokenize::Analyzer;

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Term(String),
    Prefix(String),
    Fragment(String),
    Fuzzy(String, u8),
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
    /// Slots at fixed word offsets (the first at 0), each matched by any of
    /// its alternatives (single-term items), with up to `slop` extra words.
    Phrase {
        slots: Vec<Slot>,
        slop: u32,
    },
    /// `right` after `left` (or either order, unless `ordered`) with at most
    /// `gap` words between.
    Near {
        left: Box<Query>,
        right: Box<Query>,
        gap: u32,
        ordered: bool,
    },
    /// At least `min` of the items (2 ≤ `min` < `of.len()`).
    AtLeast {
        min: usize,
        of: Vec<Query>,
    },
    /// Scales the item's weight in scores; matching is unchanged.
    Boost(Box<Query>, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub offset: u32,
    /// Terms or patterns.
    pub alts: Vec<Query>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// Nothing searchable in the query.
    Empty,
    UnbalancedParens,
    /// A negation with nothing positive to subtract from (`-foo`, `a OR -b`).
    UnboundedNegation,
    /// A `*` / `~` pattern that isn't exactly one term.
    BadPattern(String),
    /// Anything else malformed, described.
    Syntax(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            QueryError::Empty => "query has no searchable terms",
            QueryError::UnbalancedParens => "unbalanced parentheses or brackets",
            QueryError::UnboundedNegation => "negation needs a positive term beside it",
            QueryError::BadPattern(w) => {
                return write!(f, "pattern {w:?} must be one term: word*, *fragment*, or word~ / word~2");
            }
            QueryError::Syntax(m) => m,
        })
    }
}

impl std::error::Error for QueryError {}

fn syntax<T>(m: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError::Syntax(m.into()))
}

/// One item inside quotes.
#[derive(Debug, Clone, PartialEq)]
enum PhraseItem {
    Word(String),
    Gap,
    Alts(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Neg,
    And,
    Or,
    /// `THEN/N` (ordered) or `NEAR/N`.
    Near(u32, bool),
    Boost(f32),
    Phrase(Vec<PhraseItem>, u32),
    Word(String),
}

fn lex(input: &str) -> Result<Vec<Tok>, QueryError> {
    let mut toks = Vec::new();
    let mut word = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '"' => {
                flush(&mut word, &mut toks)?;
                let items = lex_phrase(&chars, &mut i)?;
                let slop = if chars.get(i) == Some(&'~') {
                    i += 1;
                    let digits = take_while(&chars, &mut i, |c| c.is_ascii_digit());
                    digits.parse().or_else(|_| syntax("a phrase's ~ needs a number: \"...\"~2"))?
                } else {
                    0
                };
                toks.push(Tok::Phrase(items, slop));
            }
            '(' | ')' | '[' | ']' => {
                flush(&mut word, &mut toks)?;
                toks.push(match c {
                    '(' => Tok::LParen,
                    ')' => Tok::RParen,
                    '[' => Tok::LBracket,
                    _ => Tok::RBracket,
                });
            }
            '^' => {
                if word.is_empty() && (i < 2 || chars[i - 2].is_whitespace()) {
                    return syntax("^ must follow an item with no space: beer^2");
                }
                flush(&mut word, &mut toks)?;
                let n = take_while(&chars, &mut i, |c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E'));
                match n.parse::<f32>() {
                    Ok(b) if (0.0..=10000.0).contains(&b) => toks.push(Tok::Boost(b)),
                    _ => return syntax(format!("boost ^{n} must be a number from 0 to 10000")),
                }
            }
            // A comma separates items, except inside a number (1,000).
            ',' if !(word.ends_with(|c: char| c.is_ascii_digit())
                && chars.get(i).is_some_and(|c| c.is_ascii_digit())) =>
            {
                flush(&mut word, &mut toks)?
            }
            '-' if word.is_empty() => toks.push(Tok::Neg),
            c if c.is_whitespace() => flush(&mut word, &mut toks)?,
            c => word.push(c),
        }
    }
    flush(&mut word, &mut toks)?;
    Ok(toks)
}

fn take_while(chars: &[char], i: &mut usize, f: impl Fn(char) -> bool) -> String {
    let start = *i;
    while *i < chars.len() && f(chars[*i]) {
        *i += 1;
    }
    chars[start..*i].iter().collect()
}

fn flush(word: &mut String, toks: &mut Vec<Tok>) -> Result<(), QueryError> {
    if word.is_empty() {
        return Ok(());
    }
    let w = std::mem::take(word);
    toks.push(match w.as_str() {
        "AND" => Tok::And,
        "OR" => Tok::Or,
        "NOT" => Tok::Neg,
        "THEN" | "NEAR" => return syntax(format!("{w} needs a distance: {w}/N")),
        _ => match w.split_once('/') {
            Some((op @ ("THEN" | "NEAR"), n)) => match n.parse() {
                Ok(n) => Tok::Near(n, op == "THEN"),
                Err(_) => return syntax(format!("{w}: the distance must be a number")),
            },
            _ => Tok::Word(w),
        },
    });
    Ok(())
}

/// Lex a phrase's body; `i` is just past the opening quote.
fn lex_phrase(chars: &[char], i: &mut usize) -> Result<Vec<PhraseItem>, QueryError> {
    let mut items = Vec::new();
    let mut word = String::new();
    let mut alts: Option<Vec<String>> = None;
    // (word, escaped) -> item
    let push = |word: &mut String,
                escaped_gap: bool,
                items: &mut Vec<PhraseItem>,
                alts: &mut Option<Vec<String>>| {
        if word.is_empty() {
            return;
        }
        let w = std::mem::take(word);
        match alts {
            Some(a) => a.push(w),
            None if w == "_" && !escaped_gap => items.push(PhraseItem::Gap),
            None => items.push(PhraseItem::Word(w)),
        }
    };
    let mut escaped_gap = false;
    loop {
        let Some(&c) = chars.get(*i) else { return syntax("unterminated phrase: missing closing \"") };
        *i += 1;
        match c {
            '\\' => match chars.get(*i) {
                Some(&e @ ('"' | '\\' | '_' | '[' | ']')) => {
                    *i += 1;
                    escaped_gap |= e == '_';
                    word.push(e);
                }
                _ => word.push('\\'),
            },
            '"' => {
                push(&mut word, escaped_gap, &mut items, &mut alts);
                if alts.is_some() {
                    return syntax("unclosed [ in a phrase");
                }
                if items.is_empty() {
                    return syntax("empty phrase");
                }
                return Ok(items);
            }
            '[' => {
                push(&mut word, escaped_gap, &mut items, &mut alts);
                escaped_gap = false;
                if alts.is_some() {
                    return syntax("[ ] cannot nest in a phrase");
                }
                alts = Some(Vec::new());
            }
            ']' => {
                push(&mut word, escaped_gap, &mut items, &mut alts);
                escaped_gap = false;
                match alts.take() {
                    Some(a) if !a.is_empty() => items.push(PhraseItem::Alts(a)),
                    Some(_) => return syntax("empty [ ] in a phrase"),
                    None => return syntax("unbalanced ] in a phrase"),
                }
            }
            c if c.is_whitespace() || (c == ',' && alts.is_some()) => {
                push(&mut word, escaped_gap, &mut items, &mut alts);
                escaped_gap = false;
            }
            c => word.push(c),
        }
    }
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

    fn peek_word(&self, ahead: usize) -> Option<&str> {
        match self.toks.get(self.pos + ahead) {
            Some(Tok::Word(w)) => Some(w),
            _ => None,
        }
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
                None | Some(Tok::RParen | Tok::RBracket | Tok::Or) => break,
                Some(Tok::And) => self.pos += 1,
                Some(Tok::Neg) => {
                    self.pos += 1;
                    match self.peek() {
                        // A stray `-` / `NOT` with nothing to negate is ignored.
                        None | Some(Tok::RParen | Tok::RBracket | Tok::Or | Tok::And) => {}
                        _ => parts.extend(self.near_expr()?.map(|q| Query::Not(Box::new(q)))),
                    }
                }
                _ => parts.extend(self.near_expr()?),
            }
        }
        Ok(collapse(parts, Query::And))
    }

    /// Items joined by `THEN/N` / `NEAR/N`, left to right.
    fn near_expr(&mut self) -> Result<Option<Query>, QueryError> {
        let mut left = self.boosted()?;
        while let Some(&Tok::Near(gap, ordered)) = self.peek() {
            self.pos += 1;
            let op = if ordered { "THEN" } else { "NEAR" };
            let right = match self.peek() {
                None | Some(Tok::RParen | Tok::RBracket | Tok::Or | Tok::And | Tok::Near(..)) => {
                    return syntax(format!("{op}/{gap} needs an item on its right"))
                }
                _ => self.boosted()?,
            };
            left = match (left, right) {
                (Some(l), Some(r)) => {
                    Some(Query::Near { left: Box::new(l), right: Box::new(r), gap, ordered })
                }
                _ => return syntax(format!("both sides of {op}/{gap} need a searchable term")),
            };
        }
        Ok(left)
    }

    fn boosted(&mut self) -> Result<Option<Query>, QueryError> {
        let q = self.primary()?;
        if let Some(&Tok::Boost(b)) = self.peek() {
            self.pos += 1;
            return Ok(q.map(|q| Query::Boost(Box::new(q), b)));
        }
        Ok(q)
    }

    fn primary(&mut self) -> Result<Option<Query>, QueryError> {
        let Some(tok) = self.toks.get(self.pos).cloned() else { return syntax("the query ends too early") };
        self.pos += 1;
        match tok {
            Tok::LParen => {
                let q = self.or_expr()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(QueryError::UnbalancedParens);
                }
                self.pos += 1;
                Ok(q)
            }
            Tok::LBracket => Ok(collapse(self.list()?, Query::Or)),
            Tok::Phrase(items, slop) => self.phrase(&items, slop),
            Tok::Word(w) if w == "ALL" && self.peek_word(0) == Some("OF") => {
                self.pos += 1;
                self.expect_list("ALL OF")?;
                Ok(collapse(self.list()?, Query::And))
            }
            Tok::Word(w)
                if w == "AT" && self.peek_word(0) == Some("LEAST") && self.peek_word(2) == Some("OF") =>
            {
                let n = self.peek_word(1).unwrap().to_owned();
                self.pos += 3;
                self.expect_list("AT LEAST n OF")?;
                let items = self.list()?;
                let min = match n.strip_suffix('%') {
                    Some(p) => match p.parse::<f64>() {
                        Ok(p) if (0.0..=100.0).contains(&p) => {
                            (p / 100.0 * items.len() as f64).ceil() as usize
                        }
                        _ => return syntax(format!("AT LEAST {n} OF: a percentage from 0% to 100%")),
                    },
                    None => n.parse().or_else(|_| syntax(format!("AT LEAST {n} OF: not a number")))?,
                };
                Ok(match min {
                    0 => return syntax("AT LEAST 0 OF matches everything; use a positive count"),
                    1 => collapse(items, Query::Or),
                    m if m >= items.len() => collapse(items, Query::And),
                    min => Some(Query::AtLeast { min, of: items }),
                })
            }
            Tok::Word(w) => self.word(&w),
            Tok::RParen | Tok::RBracket => Err(QueryError::UnbalancedParens),
            Tok::Near(g, o) => {
                syntax(format!("{}/{g} needs an item on its left", if o { "THEN" } else { "NEAR" }))
            }
            Tok::Boost(_) => syntax("^ must follow an item with no space: beer^2"),
            Tok::Neg | Tok::And | Tok::Or => unreachable!("handled by callers"),
        }
    }

    fn expect_list(&mut self, what: &str) -> Result<(), QueryError> {
        if self.peek() != Some(&Tok::LBracket) {
            return syntax(format!("{what} needs a [list]"));
        }
        self.pos += 1;
        Ok(())
    }

    /// The items of a `[list]`, after its `[`.
    fn list(&mut self) -> Result<Vec<Query>, QueryError> {
        let mut items = Vec::new();
        let mut any = false;
        loop {
            match self.peek() {
                Some(Tok::RBracket) => {
                    self.pos += 1;
                    if !any {
                        return syntax("empty [ ]");
                    }
                    return Ok(items);
                }
                None => return Err(QueryError::UnbalancedParens),
                Some(Tok::And | Tok::Or | Tok::Neg) => {
                    return syntax("AND / OR / NOT inside [ ] need parentheses: [a (b OR c)]")
                }
                _ => {
                    any = true;
                    items.extend(self.near_expr()?);
                }
            }
        }
    }

    fn phrase(&mut self, items: &[PhraseItem], slop: u32) -> Result<Option<Query>, QueryError> {
        let mut slots: Vec<Slot> = Vec::new();
        let mut offset = 0u32;
        for item in items {
            match item {
                PhraseItem::Gap => {
                    if !slots.is_empty() {
                        offset += 1; // leading gaps are ignored, trailing ones dropped below
                    }
                }
                PhraseItem::Word(w) => match self.word(w)? {
                    None => {}
                    Some(Query::Phrase { slots: inner, .. }) => {
                        for s in inner {
                            slots.push(Slot { offset: offset + s.offset, alts: s.alts });
                        }
                        offset = slots.last().unwrap().offset + 1;
                    }
                    Some(leaf) => {
                        slots.push(Slot { offset, alts: vec![leaf] });
                        offset += 1;
                    }
                },
                PhraseItem::Alts(ws) => {
                    let mut alts = Vec::new();
                    for w in ws {
                        match self.word(w)? {
                            None => {}
                            Some(Query::Phrase { .. }) => {
                                return syntax(format!("{w:?}: a [ ] choice in a phrase must be one word"))
                            }
                            Some(leaf) => alts.push(leaf),
                        }
                    }
                    if !alts.is_empty() {
                        slots.push(Slot { offset, alts });
                        offset += 1;
                    }
                }
            }
        }
        Ok(match slots.len() {
            0 => None,
            1 => {
                let mut alts = slots.pop().unwrap().alts;
                if alts.len() == 1 {
                    alts.pop()
                } else {
                    Some(Query::Or(alts))
                }
            }
            _ => Some(Query::Phrase { slots, slop }),
        })
    }

    /// A plain word (analyzed, possibly into a phrase of terms) or a pattern.
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
            let mut terms = self.analyzer.terms(w);
            return Ok(match terms.len() {
                0 => None,
                1 => terms.pop().map(Query::Term),
                _ => Some(Query::Phrase {
                    slots: (0..)
                        .zip(terms)
                        .map(|(offset, t)| Slot { offset, alts: vec![Query::Term(t)] })
                        .collect(),
                    slop: 0,
                }),
            });
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
        let q = q.ok_or(QueryError::Empty)?;
        // Rejects what no index plan can answer (a negation alone).
        Plan::from_query(&q)?;
        Ok(q)
    }

    /// Whether answering needs word positions or counts the index doesn't
    /// have (so index results are candidates).
    pub fn is_positional(&self) -> bool {
        match self {
            Query::Term(_) | Query::Prefix(_) | Query::Fragment(_) | Query::Fuzzy(..) => false,
            Query::Phrase { .. } | Query::Near { .. } | Query::AtLeast { .. } => true,
            Query::And(cs) | Query::Or(cs) => cs.iter().any(Query::is_positional),
            Query::Not(q) | Query::Boost(q, _) => q.is_positional(),
        }
    }

    /// Whether `text` matches: the exact answer, with word positions (the
    /// recheck and sequential-scan path).
    pub fn matches_text(&self, text: &str, analyzer: &mut Analyzer) -> bool {
        crate::span::DocWords::new(text, analyzer).matches(self)
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
    /// A superset of the query's matches (phrases, proximity): candidates
    /// to check against the row.
    Recheck(Box<Plan>),
}

impl Plan {
    pub fn parse(input: &str, analyzer: &mut Analyzer) -> Result<Plan, QueryError> {
        Plan::from_query(&Query::parse(input, analyzer)?)
    }

    /// The index plan: exact for term-level queries; otherwise a superset
    /// (phrases and proximity need their terms; `AT LEAST` needs any item),
    /// wrapped in [`Plan::Recheck`].
    pub fn from_query(q: &Query) -> Result<Plan, QueryError> {
        Ok(match Self::superset(q)? {
            p if q.is_positional() && !p.needs_recheck() => Plan::Recheck(Box::new(p)),
            p => p,
        })
    }

    /// A plan matching every document `q` matches.
    fn superset(q: &Query) -> Result<Plan, QueryError> {
        match q {
            Query::Term(t) => Ok(Plan::Term(t.clone())),
            Query::Prefix(t) => Ok(Plan::Prefix(t.clone())),
            Query::Fragment(t) => Ok(Plan::Fragment(t.clone())),
            Query::Fuzzy(t, k) => Ok(Plan::Fuzzy(t.clone(), *k)),
            Query::Not(_) => Err(QueryError::UnboundedNegation),
            Query::Boost(q, _) => Self::superset(q),
            Query::Or(cs) => Ok(Plan::Or(cs.iter().map(Self::superset).collect::<Result<_, _>>()?)),
            Query::AtLeast { of, .. } => {
                Ok(Plan::Or(of.iter().map(Self::superset).collect::<Result<_, _>>()?))
            }
            Query::Phrase { slots, .. } => {
                let mut per_slot = Vec::with_capacity(slots.len());
                for slot in slots {
                    let alts = slot.alts.iter().map(Self::superset).collect::<Result<_, _>>()?;
                    per_slot.push(collapse_plan(alts, Plan::Or));
                }
                Ok(Plan::And(per_slot))
            }
            Query::Near { left, right, .. } => {
                Ok(Plan::And(vec![Self::superset(left)?, Self::superset(right)?]))
            }
            Query::And(cs) => {
                let mut pos = Vec::new();
                let mut neg = Vec::new();
                for c in cs {
                    match c {
                        Query::Not(inner) => neg.extend(Self::subset(inner)),
                        other => pos.push(Self::superset(other)?),
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

    /// A plan matching only documents `q` matches (for negations), or
    /// `None` if the index can't tell any (phrases, proximity).
    fn subset(q: &Query) -> Option<Plan> {
        match q {
            Query::Term(_) | Query::Prefix(_) | Query::Fragment(_) | Query::Fuzzy(..) => {
                Self::superset(q).ok()
            }
            Query::Phrase { .. } | Query::Near { .. } | Query::Not(_) => None,
            Query::Boost(q, _) => Self::subset(q),
            Query::Or(cs) => {
                let v: Vec<Plan> = cs.iter().filter_map(Self::subset).collect();
                (!v.is_empty()).then(|| collapse_plan(v, Plan::Or))
            }
            Query::And(cs) | Query::AtLeast { of: cs, .. } => {
                if cs.iter().any(|c| matches!(c, Query::Not(_))) {
                    return None;
                }
                let v: Option<Vec<Plan>> = cs.iter().map(Self::subset).collect();
                v.map(|v| collapse_plan(v, Plan::And))
            }
        }
    }
}

fn collapse_plan(mut v: Vec<Plan>, wrap: fn(Vec<Plan>) -> Plan) -> Plan {
    if v.len() == 1 {
        v.pop().unwrap()
    } else {
        wrap(v)
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
            Plan::Recheck(p) => p.matches(doc),
        }
    }

    /// Analyze `text` and evaluate against it. Exact unless the plan holds a
    /// [`Plan::Recheck`]; the exact answer is [`Query::matches_text`].
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
            Plan::Recheck(_) => true,
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
        // A hyphen inside a word is not negation; the word is a phrase.
        assert_eq!(plan("e-mail").unwrap(), Plan::Recheck(Box::new(Plan::And(vec![t("e"), t("mail")]))));
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
        for bad in [
            "\"a b",
            "\"\"",
            "a THEN b",
            "a NEAR/x b",
            "a THEN/2",
            "THEN/2 b",
            "a^x",
            "a ^2",
            "[]",
            "[a b",
            "AT LEAST 0 OF [a b]",
            "AT LEAST 2 OF a",
            "[a OR b]",
            "\"a [b c\"",
        ] {
            assert!(matches!(plan(bad), Err(QueryError::Syntax(_) | QueryError::UnbalancedParens)), "{bad}");
        }
    }

    fn q(s: &str) -> Query {
        Query::parse(s, &mut Analyzer::new()).unwrap()
    }
    fn qt(s: &str) -> Query {
        Query::Term(s.into())
    }
    fn phrase(terms: &[(&str, u32)], slop: u32) -> Query {
        let slots = terms.iter().map(|&(t, offset)| Slot { offset, alts: vec![qt(t)] }).collect();
        Query::Phrase { slots, slop }
    }

    #[test]
    fn tinql_syntax() {
        assert_eq!(q("\"Big bad wolf\""), phrase(&[("big", 0), ("bad", 1), ("wolf", 2)], 0));
        assert_eq!(q("\"_ big _ _ wolf _\"~3"), phrase(&[("big", 0), ("wolf", 3)], 3));
        assert_eq!(q("e-mail"), phrase(&[("e", 0), ("mail", 1)], 0));
        assert_eq!(q("\"fast e-mail\""), phrase(&[("fast", 0), ("e", 1), ("mail", 2)], 0));
        assert_eq!(
            q("\"[big, large] wolf\""),
            Query::Phrase {
                slots: vec![
                    Slot { offset: 0, alts: vec![qt("big"), qt("large")] },
                    Slot { offset: 1, alts: vec![qt("wolf")] }
                ],
                slop: 0
            }
        );
        assert_eq!(q("\"wolf\""), qt("wolf"));
        assert_eq!(q("[beer, ale lager]"), Query::Or(vec![qt("beer"), qt("ale"), qt("lager")]));
        assert_eq!(q("1,000"), qt("1,000"));
        assert_eq!(q("ALL OF [a b]"), Query::And(vec![qt("a"), qt("b")]));
        assert_eq!(q("AT LEAST 1 OF [a b]"), Query::Or(vec![qt("a"), qt("b")]));
        assert_eq!(
            q("AT LEAST 2 OF [a b c]"),
            Query::AtLeast { min: 2, of: vec![qt("a"), qt("b"), qt("c")] }
        );
        assert_eq!(
            q("AT LEAST 50% OF [a b c]"),
            Query::AtLeast { min: 2, of: vec![qt("a"), qt("b"), qt("c")] }
        );
        // Lower-case keywords are words.
        assert_eq!(q("at least of"), Query::And(vec![qt("at"), qt("least"), qt("of")]));
        let near = |l, r, gap, ordered| Query::Near { left: Box::new(l), right: Box::new(r), gap, ordered };
        assert_eq!(q("a THEN/2 b NEAR/5 c"), near(near(qt("a"), qt("b"), 2, true), qt("c"), 5, false));
        // Proximity binds tighter than AND, AND than OR.
        assert_eq!(
            q("x OR a THEN/1 b c"),
            Query::Or(vec![qt("x"), Query::And(vec![near(qt("a"), qt("b"), 1, true), qt("c")])])
        );
        assert_eq!(q("beer^2"), Query::Boost(Box::new(qt("beer")), 2.0));
        assert_eq!(q("\"a b\"^0.5"), Query::Boost(Box::new(phrase(&[("a", 0), ("b", 1)], 0)), 0.5));
        assert_eq!(q("a AND NOT b"), Query::And(vec![qt("a"), Query::Not(Box::new(qt("b")))]));
        assert_eq!(q("\"say \\\"hi\\\"\""), phrase(&[("say", 0), ("hi", 1)], 0));
    }

    #[test]
    fn positional_plans() {
        // Phrases and proximity: their terms, rechecked.
        let rc = |p| Plan::Recheck(Box::new(p));
        assert_eq!(plan("\"a b\"").unwrap(), rc(Plan::And(vec![t("a"), t("b")])));
        assert_eq!(plan("\"[a c] b\"").unwrap(), rc(Plan::And(vec![Plan::Or(vec![t("a"), t("c")]), t("b")])));
        assert_eq!(plan("a NEAR/3 b").unwrap(), rc(Plan::And(vec![t("a"), t("b")])));
        assert_eq!(plan("AT LEAST 2 OF [a b c]").unwrap(), rc(Plan::Or(vec![t("a"), t("b"), t("c")])));
        // Under a negation the index can only subtract what is certain.
        assert_eq!(plan("a -\"b c\"").unwrap(), rc(t("a")));
        assert_eq!(
            plan("a -AT LEAST 2 OF [b c d]").unwrap(),
            rc(Plan::AndNot(Box::new(t("a")), Box::new(Plan::And(vec![t("b"), t("c"), t("d")]))))
        );
        assert_eq!(plan("a -(b OR \"c d\")").unwrap(), rc(Plan::AndNot(Box::new(t("a")), Box::new(t("b")))));
        assert!(!plan("a b^2 -c").unwrap().needs_recheck());
    }
}
