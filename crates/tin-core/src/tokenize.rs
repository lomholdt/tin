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
        self.for_each_token(text, |t, pos, _| f(t, pos));
    }

    /// Like [`for_each_term`](Self::for_each_term), also giving each term's
    /// byte range in `text` (for highlighting).
    pub fn for_each_token(&mut self, text: &str, mut f: impl FnMut(&str, u32, std::ops::Range<usize>)) {
        let mut pos = 0u32;
        for_each_word(text, |at, word| {
            fold_into(word, &mut self.buf);
            if !self.buf.is_empty() && self.buf.len() <= MAX_TERM_BYTES {
                f(&self.buf, pos, at..at + word.len());
            }
            pos += 1;
        });
    }

    /// Like [`for_each_term`](Self::for_each_term), but words for which
    /// `keep(raw_word)` is false are skipped before folding (they still take
    /// a position). Returns the number of word positions.
    pub fn for_each_term_if(
        &mut self,
        text: &str,
        mut keep: impl FnMut(&str) -> bool,
        mut f: impl FnMut(&str, u32),
    ) -> u32 {
        let mut pos = 0u32;
        for_each_word(text, |_, word| {
            if keep(word) {
                fold_into(word, &mut self.buf);
                if !self.buf.is_empty() && self.buf.len() <= MAX_TERM_BYTES {
                    f(&self.buf, pos);
                }
            }
            pos += 1;
        });
        pos
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

/// Calls `f(byte_offset, word)` for each word of `text`, exactly as
/// `unicode_word_indices` would (UAX #29 word boundaries, words with a letter
/// or digit), but several times faster on ASCII.
///
/// The text is cut where ASCII whitespace meets an ASCII non-space byte.
/// UAX #29 always breaks there (neither side joins: WB3d needs two spaces,
/// WB4 an extending mark after the cut), and no rule decides anything on
/// one side by looking past such a cut (the context rules look for letters,
/// digits, joiners, marks or regional indicators, which neither byte is).
/// So the pieces around each non-ASCII byte go through
/// `unicode-segmentation`, and everything else through [`ascii_words`], the
/// UAX #29 rules for ASCII. A differential test checks the result against
/// `unicode_word_indices` on the whole text.
pub fn for_each_word<'t>(text: &'t str, mut f: impl FnMut(usize, &'t str)) {
    let b = text.as_bytes();
    #[inline(always)]
    fn ascii<'t>(text: &'t str, from: usize, to: usize, f: &mut impl FnMut(usize, &'t str)) {
        ascii_words(&text.as_bytes()[from..to], |s, e| f(from + s, &text[from + s..from + e]));
    }
    let mut pos = 0;
    while pos < b.len() {
        let Some(q) = b[pos..].iter().position(|&c| !c.is_ascii()).map(|q| pos + q) else {
            ascii(text, pos, b.len(), &mut f);
            return;
        };
        // The piece around q: from the last cut at or before it, to the
        // first cut after it.
        let mut lo = q;
        while lo > pos && !is_cut(b, lo) {
            lo -= 1;
        }
        let mut hi = q + 1;
        while hi < b.len() && !is_cut(b, hi) {
            hi += 1;
        }
        ascii(text, pos, lo, &mut f);
        for (at, w) in text[lo..hi].unicode_word_indices() {
            f(lo + at, w);
        }
        pos = hi;
    }
}

/// Whether text may be cut before byte `i` (see [`for_each_word`]).
#[inline]
fn is_cut(b: &[u8], i: usize) -> bool {
    is_space(b[i - 1]) && b[i].is_ascii() && !is_space(b[i])
}

/// ASCII whitespace (UAX #29 WSegSpace, CR, LF, Newline, or tab).
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// UAX #29 word-break classes of ASCII bytes.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Wb {
    /// ALetter
    Letter,
    Numeric,
    /// ExtendNumLet: `_`
    Connector,
    /// MidLetter: `:`
    MidLetter,
    /// MidNumLet and Single_Quote: `.` `'`
    MidNumLet,
    /// MidNum: `,` `;`
    MidNum,
    Other,
}

const fn wb_of(b: u8) -> Wb {
    match b {
        b'a'..=b'z' | b'A'..=b'Z' => Wb::Letter,
        b'0'..=b'9' => Wb::Numeric,
        b'_' => Wb::Connector,
        b':' => Wb::MidLetter,
        b'.' | b'\'' => Wb::MidNumLet,
        b',' | b';' => Wb::MidNum,
        _ => Wb::Other,
    }
}

/// The words of an ASCII string: `f(start, end)` per word. Letters, digits
/// and `_` always join (WB5, WB8–WB10, WB13a/b); a letter-`:`/`.`/`'`-letter
/// or digit-`,`/`;`/`.`/`'`-digit sequence joins across the middle
/// character (WB6/7, WB11/12); everything else breaks (WB999). Runs of `_`
/// alone are not words.
///
/// Branch-light: each 64-byte block becomes bitmasks (letters, digits, word
/// bytes, joiners; with AVX2 when the CPU has it), the joined middle characters are found with shifts
/// (every join rule is local: it looks at one byte on each side), and word
/// starts and ends are the edges of the resulting runs, taken with
/// trailing-zero counts. A word costs a few instructions instead of the
/// mispredicted branches of a byte loop.
pub fn ascii_words(b: &[u8], mut f: impl FnMut(usize, usize)) {
    let blocks = b.len().div_ceil(64);
    if blocks == 0 {
        return;
    }
    let classify = Masks::classifier();
    let mut cur = classify(b, 0);
    let mut prev = Masks::default(); // before the text: nothing
    let mut prev_m63 = 0u64; // whether the previous block's last byte is in a word
    let mut pending: Option<usize> = None;
    for k in 0..blocks {
        let next = if k + 1 < blocks { classify(b, (k + 1) * 64) } else { Masks::default() };
        let m = cur.word_bytes(prev.l >> 63, prev.d >> 63, next.l & 1, next.d & 1);
        // Whether the next block's first byte is in a word (a joiner there
        // needs this block's last byte and its own second byte).
        let next_m0 = next.word_bytes(cur.l >> 63, cur.d >> 63, 0, 0) & 1;
        let mut starts = m & !((m << 1) | prev_m63);
        let mut ends = m & !((m >> 1) | (next_m0 << 63));
        let base = k * 64;
        loop {
            if pending.is_none() {
                if starts == 0 {
                    break;
                }
                pending = Some(base + starts.trailing_zeros() as usize);
                starts &= starts - 1;
            }
            if ends == 0 {
                break; // the word goes on in the next block
            }
            let end = base + ends.trailing_zeros() as usize + 1;
            ends &= ends - 1;
            let start = pending.take().unwrap();
            // A run of `_` alone is not a word.
            if b[start] != b'_' || b[start..end].iter().any(|&c| c != b'_') {
                f(start, end);
            }
        }
        prev_m63 = m >> 63;
        prev = cur;
        cur = next;
    }
}

/// Byte classes of one 64-byte block, one bit per byte.
#[derive(Copy, Clone, Default)]
struct Masks {
    /// Letters.
    l: u64,
    /// Digits.
    d: u64,
    /// Letters, digits and `_`.
    w: u64,
    /// Joins letters: `:` `.` `'`.
    mid_l: u64,
    /// Joins digits: `,` `;` `.` `'`.
    mid_n: u64,
}

const C_LETTER: u8 = 1;
const C_DIGIT: u8 = 2;
const C_CONNECTOR: u8 = 4;
const C_MID_LETTER: u8 = 8;
const C_MID_NUM: u8 = 16;

const CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = match wb_of(i as u8) {
            Wb::Letter => C_LETTER,
            Wb::Numeric => C_DIGIT,
            Wb::Connector => C_CONNECTOR,
            Wb::MidLetter => C_MID_LETTER,
            Wb::MidNumLet => C_MID_LETTER | C_MID_NUM,
            Wb::MidNum => C_MID_NUM,
            Wb::Other => 0,
        };
        i += 1;
    }
    t
};

impl Masks {
    /// The fastest classifier this CPU has.
    fn classifier() -> fn(&[u8], usize) -> Masks {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 is available.
            return |b, at| unsafe { Masks::of_avx2(b, at) };
        }
        Masks::of
    }

    /// The block of `b` starting at `at` (bytes past the end: none), with
    /// AVX2: range and equality compares on 32 bytes at a time, then one
    /// bit per byte with `movemask`. Only called on ASCII text.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn of_avx2(b: &[u8], at: usize) -> Masks {
        use std::arch::x86_64::*;
        let mut tail = [0u8; 64];
        let block: &[u8] = if at + 64 <= b.len() {
            &b[at..at + 64]
        } else {
            tail[..b.len() - at].copy_from_slice(&b[at..]);
            &tail
        };
        let half = |x: __m256i| -> [u32; 5] {
            let set = |c: u8| _mm256_set1_epi8(c as i8);
            let lower = _mm256_or_si256(x, set(0x20));
            // ASCII bytes compare the same signed or unsigned.
            let letter = _mm256_and_si256(
                _mm256_cmpgt_epi8(lower, set(b'a' - 1)),
                _mm256_cmpgt_epi8(set(b'z' + 1), lower),
            );
            let digit =
                _mm256_and_si256(_mm256_cmpgt_epi8(x, set(b'0' - 1)), _mm256_cmpgt_epi8(set(b'9' + 1), x));
            let under = _mm256_cmpeq_epi8(x, set(b'_'));
            let dot_quote =
                _mm256_or_si256(_mm256_cmpeq_epi8(x, set(b'.')), _mm256_cmpeq_epi8(x, set(b'\'')));
            let mid_l = _mm256_or_si256(dot_quote, _mm256_cmpeq_epi8(x, set(b':')));
            let mid_n = _mm256_or_si256(
                dot_quote,
                _mm256_or_si256(_mm256_cmpeq_epi8(x, set(b',')), _mm256_cmpeq_epi8(x, set(b';'))),
            );
            let bits = |m: __m256i| _mm256_movemask_epi8(m) as u32;
            [bits(letter), bits(digit), bits(under), bits(mid_l), bits(mid_n)]
        };
        let lo = half(_mm256_loadu_si256(block.as_ptr() as *const __m256i));
        let hi = half(_mm256_loadu_si256(block.as_ptr().add(32) as *const __m256i));
        let join = |i: usize| lo[i] as u64 | (hi[i] as u64) << 32;
        let (l, d) = (join(0), join(1));
        Masks { l, d, w: l | d | join(2), mid_l: join(3), mid_n: join(4) }
    }

    /// The block of `b` starting at `at` (bytes past the end: none).
    fn of(b: &[u8], at: usize) -> Masks {
        let block = &b[at..(at + 64).min(b.len())];
        let (mut l, mut d, mut c, mut ml, mut mn) = (0u64, 0u64, 0u64, 0u64, 0u64);
        for (i, &byte) in block.iter().enumerate() {
            let k = CLASS[byte as usize] as u64;
            l |= (k & 1) << i;
            d |= ((k >> 1) & 1) << i;
            c |= ((k >> 2) & 1) << i;
            ml |= ((k >> 3) & 1) << i;
            mn |= ((k >> 4) & 1) << i;
        }
        Masks { l, d, w: l | d | c, mid_l: ml, mid_n: mn }
    }

    /// Bytes inside words: word bytes, plus joiners between two letters or
    /// two digits. `*_before` / `*_after`: the class bits of the bytes just
    /// outside the block.
    fn word_bytes(&self, l_before: u64, d_before: u64, l_after: u64, d_after: u64) -> u64 {
        let l_prev = (self.l << 1) | l_before;
        let l_next = (self.l >> 1) | (l_after << 63);
        let d_prev = (self.d << 1) | d_before;
        let d_next = (self.d >> 1) | (d_after << 63);
        self.w | (l_prev & self.mid_l & l_next) | (d_prev & self.mid_n & d_next)
    }
}

/// Case- and accent-fold one word into `out` (cleared first).
fn fold_into(word: &str, out: &mut String) {
    out.clear();
    if word.is_ascii() {
        out.push_str(word);
        out.make_ascii_lowercase();
        return;
    }
    for c in word.nfkd().filter(|&c| !is_combining_mark(c)) {
        out.extend(c.to_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift, for the differential test.
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    fn ours(text: &str) -> Vec<(usize, String)> {
        let mut v = Vec::new();
        for_each_word(text, |at, w| v.push((at, w.to_owned())));
        v
    }

    fn reference(text: &str) -> Vec<(usize, String)> {
        text.unicode_word_indices().map(|(at, w)| (at, w.to_owned())).collect()
    }

    #[test]
    fn classifiers_agree() {
        let mut rng = Rng(7);
        for _ in 0..20_000 {
            let len = rng.below(150);
            let b: Vec<u8> = (0..len).map(|_| rng.below(128) as u8).collect();
            let classify = Masks::classifier();
            for at in (0..len).step_by(64) {
                let (x, y) = (classify(&b, at), Masks::of(&b, at));
                assert_eq!(
                    (x.l, x.d, x.w, x.mid_l, x.mid_n),
                    (y.l, y.d, y.w, y.mid_l, y.mid_n),
                    "{b:?} at {at}"
                );
            }
        }
    }

    #[test]
    fn words_match_unicode_segmentation() {
        // Every ASCII byte, plus non-ASCII letters, marks, joiners, quotes,
        // spaces and symbols that take part in UAX #29 rules.
        let mut alphabet: Vec<String> = (0u8..128).map(|b| (b as char).to_string()).collect();
        for s in [
            "é",
            "ü",
            "Å",
            "ß",
            "中",
            "日",
            "カ",
            "ア",
            "א",
            "ب",
            "\u{301}",
            "\u{200d}",
            "\u{200b}",
            "\u{a0}",
            "\u{2019}",
            "\u{2018}",
            "\u{b7}",
            "\u{2024}",
            "\u{fe13}",
            "\u{ff0e}",
            "\u{1f600}",
            "\u{1f1e9}",
            "\u{1f1f0}",
            "\u{2060}",
            "\u{ad}",
            "\u{2028}",
            "\u{3000}",
            "\u{661}",
            "\u{ff11}",
            "ﬁ",
            // Alphabetic combining marks: after a space they make a word of it.
            "\u{64e}",
            "\u{345}",
            "\u{903}",
            "\u{93e}",
        ] {
            alphabet.push(s.to_owned());
        }
        // Weight the characters the ASCII rules care about.
        for s in ["a", "b", "Z", "1", "2", "_", ".", "'", ",", ";", ":", " ", "-", "\"", "é", "\u{64e}"] {
            for _ in 0..8 {
                alphabet.push(s.to_owned());
            }
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..200_000 {
            let len = rng.below(14);
            let text: String = (0..len).map(|_| alphabet[rng.below(alphabet.len())].as_str()).collect();
            assert_eq!(ours(&text), reference(&text), "{text:?}");
        }
        for text in [
            "fox's 3.14 a:b a.b.c 1,000.5 a1.5 1.a _a_ __ a__b e-mail x.y, 'quoted' 12:30 U.S.A.",
            "done. \u{64e}",
            "a  \u{64e}b\t\u{64e}\n\u{64e}",
        ] {
            assert_eq!(ours(text), reference(text), "{text:?}");
        }
    }

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
