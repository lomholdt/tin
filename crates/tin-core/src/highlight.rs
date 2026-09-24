//! Highlighting: the words of a document that made it match, wrapped in
//! markers ([`DocWords::marks`] says which).

use crate::query::Query;
use crate::span::DocWords;
use crate::tokenize::Analyzer;

/// `text` with the matched words wrapped in `start` / `stop`; consecutive
/// matched words share one pair (`<b>big bad wolf</b>`, `<b>e-mail</b>`)
/// unless punctuation and spaces part them.
pub fn highlight(text: &str, q: &Query, analyzer: &mut Analyzer, start: &str, stop: &str) -> String {
    let tokens = tokens(text, analyzer);
    let marks = DocWords::new(text, analyzer).marks(q);
    render(text, &tokens, &marks, 0..text.len(), start, stop)
}

/// Up to `words` words of `text` around its densest run of matched words,
/// highlighted, with `…` where text was cut. The start of the text if
/// nothing matched.
pub fn snippet(
    text: &str,
    q: &Query,
    analyzer: &mut Analyzer,
    start: &str,
    stop: &str,
    words: usize,
) -> String {
    let tokens = tokens(text, analyzer);
    if tokens.is_empty() || words == 0 {
        return String::new();
    }
    let marks = DocWords::new(text, analyzer).marks(q);
    let words = words.min(tokens.len());
    // Window over token indexes with the most marks (earliest on ties).
    let marked: Vec<bool> = tokens.iter().map(|t| marks.binary_search(&t.0).is_ok()).collect();
    let mut count: usize = marked[..words].iter().filter(|&&m| m).count();
    let (mut best, mut best_at) = (count, 0);
    for i in words..tokens.len() {
        count += marked[i] as usize;
        count -= marked[i - words] as usize;
        if count > best {
            (best, best_at) = (count, i + 1 - words);
        }
    }
    // Start the window a little before its first match, if it has room.
    if let Some(first) = marked[best_at..best_at + words].iter().position(|&m| m) {
        let lead = words / 4;
        if first > lead {
            best_at = (best_at + first - lead).min(tokens.len() - words);
        }
    }
    let (from, to) = (tokens[best_at].1.start, tokens[best_at + words - 1].1.end);
    let mut out = String::new();
    if best_at > 0 {
        out.push('…');
    }
    out.push_str(&render(text, &tokens, &marks, from..to, start, stop));
    if best_at + words < tokens.len() {
        out.push('…');
    }
    out
}

type Token = (u32, std::ops::Range<usize>);

/// Whether two matched words with `between` them share one marker: only
/// spaces (`big bad`) or no spaces at all (`e-mail`), but not `big, bad`.
fn joinable(between: &str) -> bool {
    let spaces = between.chars().filter(|c| c.is_whitespace()).count();
    spaces == 0 || spaces == between.chars().count()
}

fn tokens(text: &str, analyzer: &mut Analyzer) -> Vec<Token> {
    let mut v = Vec::new();
    analyzer.for_each_token(text, |_, pos, range| v.push((pos, range)));
    v
}

fn render(
    text: &str,
    tokens: &[Token],
    marks: &[u32],
    within: std::ops::Range<usize>,
    start: &str,
    stop: &str,
) -> String {
    let mut out = String::with_capacity(within.len() + marks.len() * (start.len() + stop.len()));
    let mut at = within.start;
    let mut i = 0;
    while i < tokens.len() {
        let (pos, ref r) = tokens[i];
        if r.start < within.start || r.end > within.end || marks.binary_search(&pos).is_err() {
            i += 1;
            continue;
        }
        // A run of consecutive marked words.
        let mut j = i;
        while j + 1 < tokens.len()
            && tokens[j + 1].0 == tokens[j].0 + 1
            && tokens[j + 1].1.end <= within.end
            && marks.binary_search(&tokens[j + 1].0).is_ok()
            && joinable(&text[tokens[j].1.end..tokens[j + 1].1.start])
        {
            j += 1;
        }
        out.push_str(&text[at..r.start]);
        out.push_str(start);
        out.push_str(&text[r.start..tokens[j].1.end]);
        out.push_str(stop);
        at = tokens[j].1.end;
        i = j + 1;
    }
    out.push_str(&text[at..within.end]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hl(q: &str, doc: &str) -> String {
        let mut a = Analyzer::new();
        highlight(doc, &Query::parse(q, &mut a).unwrap(), &mut a, "[", "]")
    }

    #[test]
    fn highlights() {
        let doc = "The big bad wolf ate a big, grey wolf.";
        assert_eq!(hl("wolf", doc), "The big bad [wolf] ate a big, grey [wolf].");
        assert_eq!(hl("\"big bad wolf\"", doc), "The [big bad wolf] ate a big, grey wolf.");
        // "big bad wolf" fits "big _ wolf" too.
        assert_eq!(hl("\"BIG _ wolf\"", doc), "The [big] bad [wolf] ate a [big], grey [wolf].");
        assert_eq!(hl("\"bad _ _ _ big\"", doc), "The big [bad] wolf ate a [big], grey wolf.");
        assert_eq!(hl("grey NEAR/0 wolf big", doc), "The [big] bad wolf ate a [big], [grey wolf].");
        assert_eq!(hl("wolf AND NOT cat", doc), "The big bad [wolf] ate a big, grey [wolf].");
        assert_eq!(hl("gr*", "Grüße, grey"), "[Grüße], [grey]");
        assert_eq!(hl("e-mail", "send e-mail now"), "send [e-mail] now");
    }

    #[test]
    fn snippets() {
        let mut a = Analyzer::new();
        let doc = (0..40).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let q = Query::parse("w20 w21", &mut a).unwrap();
        let s = snippet(&doc, &q, &mut a, "[", "]", 8);
        assert_eq!(s, "…w18 w19 [w20 w21] w22 w23 w24 w25…");
        let q = Query::parse("w0", &mut a).unwrap();
        assert_eq!(snippet(&doc, &q, &mut a, "[", "]", 3), "[w0] w1 w2…");
        assert_eq!(snippet("a b", &q, &mut a, "[", "]", 10), "a b");
    }
}
