//! Word-splitting speed and exactness on a corpus (one document per line):
//! tin's splitter (`tin_core::tokenize::for_each_word`) against
//! `unicode_word_indices`, which it must match word for word; the full
//! analyzer (with folding); and the recheck a sequential scan does.
//!
//!   cargo run --release -p tin-bench --bin tokenize -- DOCS.txt
//!
//! `TIN_LIMIT=n` reads only the first n documents; `TIN_RECHECK_ONLY=1`
//! runs only the recheck (for profilers).

use std::time::Instant;

use tin_core::span::{DocWords, Wanted};
use tin_core::tokenize::{ascii_words, for_each_word};
use tin_core::{Analyzer, Query};
use unicode_segmentation::UnicodeSegmentation;

fn main() {
    let path = std::env::args().nth(1).expect("usage: tokenize DOCS.txt");
    let text = std::fs::read_to_string(path).unwrap();
    let limit: usize = std::env::var("TIN_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let docs: Vec<&str> = text.lines().take(limit).collect();
    let mb = docs.iter().map(|d| d.len()).sum::<usize>() as f64 / 1e6;
    if std::env::var("TIN_RECHECK_ONLY").is_err() {
        split(&docs, mb);
        exact(&docs);
    }
    recheck(&docs, mb);
}

fn rate(what: &str, mb: f64, t: Instant) {
    let el = t.elapsed();
    println!("{what}: {el:.2?} ({:.0} MB/s)", mb / el.as_secs_f64());
}

fn split(docs: &[&str], mb: f64) {
    let t = Instant::now();
    let n: usize = docs.iter().map(|d| d.bytes().filter(|&b| b == b' ').count()).sum();
    rate(&format!("baseline, count spaces ({n})"), mb, t);

    let t = Instant::now();
    let n: usize = docs.iter().map(|d| d.unicode_word_indices().count()).sum();
    rate(&format!("unicode_word_indices ({n} words)"), mb, t);

    let t = Instant::now();
    let mut n = 0usize;
    for d in docs {
        for_each_word(d, |_, _| n += 1);
    }
    rate(&format!("for_each_word ({n} words)"), mb, t);

    let ascii: Vec<&str> = docs.iter().copied().filter(|d| d.is_ascii()).collect();
    let ascii_mb = ascii.iter().map(|d| d.len()).sum::<usize>() as f64 / 1e6;
    let t = Instant::now();
    let mut n = 0usize;
    for d in &ascii {
        ascii_words(d.as_bytes(), |_, _| n += 1);
    }
    rate(&format!("ascii_words on the ASCII documents ({n} words)"), ascii_mb, t);

    let t = Instant::now();
    let mut a = Analyzer::new();
    let mut n = 0usize;
    for d in docs {
        a.for_each_term(d, |_, _| n += 1);
    }
    rate(&format!("analyzer, with folding ({n} terms)"), mb, t);
}

/// Word for word against `unicode_word_indices`.
fn exact(docs: &[&str]) {
    let mut bad = 0;
    for d in docs {
        let mut ours = Vec::new();
        for_each_word(d, |at, w| ours.push((at, w)));
        let reference: Vec<(usize, &str)> = d.unicode_word_indices().collect();
        if ours != reference {
            bad += 1;
            if bad <= 3 {
                eprintln!("differs: {d:?}");
            }
        }
    }
    println!("{} documents, {bad} split differently from unicode_word_indices", docs.len());
}

fn recheck(docs: &[&str], mb: f64) {
    for q in ["\"and the\"", "and the", "craft THEN/3 beer"] {
        let mut a = Analyzer::new();
        let query = Query::parse(q, &mut a).unwrap();
        let wanted = Wanted::new(&query);
        let t = Instant::now();
        let n = docs.iter().filter(|d| DocWords::with(d, &mut a, &wanted).matches(&query)).count();
        let us = t.elapsed().as_secs_f64() * 1e6 / docs.len() as f64;
        rate(&format!("recheck {q} ({n} match, {us:.2} µs/doc)"), mb, t);
    }
}
