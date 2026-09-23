//! Analyzer: text -> terms.
//!
//! Unicode (UAX #29) word boundaries, then case folding and accent folding
//! (NFKD, combining marks dropped). No stemming and no stop words: the post
//! calls both out as precision trade-offs ("The Who"), so they stay opt-in for
//! a later phase. Positions are reported so phrase/span queries (Phase 5) can
//! reuse the same analyzer.

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
use unicode_segmentation::UnicodeSegmentation;

/// Tokens longer than this (in bytes, after folding) are dropped: they are
/// almost always base64 blobs, hashes or URLs and would bloat the dictionary.
pub const MAX_TERM_BYTES: usize = 64;

#[derive(Default)]
pub struct Analyzer {
    buf: String,
}

impl Analyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Calls `f(term, position)` for every term in `text`. `position` counts
    /// words, including dropped over-long ones, so gaps stay honest.
    pub fn for_each_term(&mut self, text: &str, mut f: impl FnMut(&str, u32)) {
        for (pos, word) in text.unicode_words().enumerate() {
            fold_into(word, &mut self.buf);
            if !self.buf.is_empty() && self.buf.len() <= MAX_TERM_BYTES {
                f(&self.buf, pos as u32);
            }
        }
    }

    /// The distinct terms of `text`, sorted.
    pub fn unique_terms(&mut self, text: &str) -> Vec<String> {
        let mut v = self.terms(text);
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Convenience for tests and query parsing.
    pub fn terms(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.for_each_term(text, |t, _| out.push(t.to_owned()));
        out
    }
}

/// Case- and accent-fold one word into `out` (cleared first).
fn fold_into(word: &str, out: &mut String) {
    out.clear();
    if word.is_ascii() {
        out.extend(word.bytes().map(|b| b.to_ascii_lowercase() as char));
        return;
    }
    for c in word.nfkd().filter(|&c| !is_combining_mark(c)) {
        out.extend(c.to_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_and_accents() {
        let mut a = Analyzer::new();
        assert_eq!(a.terms("Crème BRÛLÉE, naïve café!"), ["creme", "brulee", "naive", "cafe"]);
    }

    #[test]
    fn unicode_word_boundaries() {
        let mut a = Analyzer::new();
        assert_eq!(
            a.terms("The quick (\"brown\") fox's 3.14 jumps."),
            ["the", "quick", "brown", "fox's", "3.14", "jumps"]
        );
        // NFKD only strips marks that decompose: å -> a, but ø and æ are
        // letters in their own right. (Full ASCII folding is a later option.)
        assert_eq!(a.terms("København ÆØÅ"), ["københavn", "æøa"]);
    }

    #[test]
    fn positions_and_long_tokens() {
        let mut a = Analyzer::new();
        let long = "x".repeat(MAX_TERM_BYTES + 1);
        let text = format!("one {long} three");
        let mut seen = Vec::new();
        a.for_each_term(&text, |t, p| seen.push((t.to_owned(), p)));
        assert_eq!(seen, [("one".to_owned(), 0), ("three".to_owned(), 2)]);
    }
}
