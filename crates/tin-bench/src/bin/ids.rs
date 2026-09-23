//! Identifier search in tin-core alone (no Postgres), for profiling: builds
//! the benchmark's 5M shipments into segments with grams, the way pg_tin's
//! build does, then runs every query through the same tiers as
//! `bench/ids/pg_bench.sql`'s `search_tin` and times each tier.
//!
//!   cargo run --release -p tin-bench --bin ids -- DATA_DIR [KIND] [REPEAT]
//!
//! DATA_DIR holds `shipments.csv` and `queries.tsv` (`bench/ids/generate.py`);
//! the built segments are cached there as `tin-seg*.bin`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use tin_core::{Analyzer, Plan, Segment, SegmentBuilder, Tid};

/// Rows per heap page for these ~60-byte rows (5M rows in 66,672 pages).
const ROWS_PER_PAGE: u32 = 75;
/// Segments to build (`TIN_SEGMENTS`, default 3 like pg_tin's 5M build).
fn segments_wanted() -> u32 {
    std::env::var("TIN_SEGMENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(3)
}
const K: usize = 10;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: ids DATA_DIR [KIND] [REPEAT]");
    let only = args.next().filter(|k| k != "all");
    let repeat: usize = args.next().map_or(1, |r| r.parse().unwrap());

    let tid = |i: u32| Tid::new(i / ROWS_PER_PAGE, (i % ROWS_PER_PAGE) as u16 + 1);
    let t = Instant::now();
    #[allow(non_snake_case)]
    let SEGMENTS = segments_wanted();
    let cache = |s: u32| format!("{dir}/tin-seg{SEGMENTS}-{s}.bin");
    let segments: Vec<Segment> = if std::path::Path::new(&cache(0)).exists() {
        // Built earlier (profilers run this far slower than native).
        (0..SEGMENTS).map(|s| Segment::from_bytes(&std::fs::read(cache(s)).unwrap()).unwrap()).collect()
    } else {
        let rows: Vec<String> = std::fs::read_to_string(format!("{dir}/shipments.csv"))
            .unwrap()
            .lines()
            .map(|l| l.split_once(',').unwrap().1.replace(',', " "))
            .collect();
        let n = rows.len() as u32;
        let per = n.div_ceil(SEGMENTS * ROWS_PER_PAGE) * ROWS_PER_PAGE;
        let segments: Vec<Segment> = (0..SEGMENTS)
            .into_par_iter()
            .map(|s| {
                let (lo, hi) = (s * per, ((s + 1) * per).min(n));
                let mut b =
                    SegmentBuilder::new(lo / ROWS_PER_PAGE, hi.div_ceil(ROWS_PER_PAGE)).with_grams(true);
                for i in lo..hi {
                    b.add(tid(i), &rows[i as usize]);
                }
                b.finish()
            })
            .collect();
        for (s, seg) in segments.iter().enumerate() {
            std::fs::write(cache(s as u32), seg.to_bytes()).unwrap();
        }
        segments
    };
    // TIN_MERGE=1: time merging the segments into one.
    let segments = if std::env::var("TIN_MERGE").is_ok() {
        let t = Instant::now();
        let inputs: Vec<(&Segment, Option<&[u64]>)> = segments.iter().map(|s| (s, None)).collect();
        let merged = Segment::merge(&inputs).expect("non-empty");
        eprintln!("merged {} segments in {:.1?}", segments.len(), t.elapsed());
        vec![merged]
    } else {
        segments
    };
    let bytes: usize = segments.iter().map(|s| s.size_bytes()).sum();
    let n: u64 = segments.iter().map(|s| s.meta().doc_count).sum();
    eprintln!("{n} rows in {} segments ({} MB), ready in {:.1?}", segments.len(), bytes >> 20, t.elapsed());

    // Row texts by tid, for rechecks.
    let texts: std::collections::HashMap<Tid, String> =
        std::fs::read_to_string(format!("{dir}/shipments.csv"))
            .unwrap()
            .lines()
            .enumerate()
            .map(|(i, l)| (tid(i as u32), l.split_once(',').unwrap().1.replace(',', " ")))
            .collect();
    let mut a2 = Analyzer::new();

    let queries: Vec<(String, String)> = std::fs::read_to_string(format!("{dir}/queries.tsv"))
        .unwrap()
        .lines()
        .map(|l| {
            let mut p = l.split('\t');
            (p.next().unwrap().to_owned(), p.next().unwrap().to_owned())
        })
        .filter(|(k, _)| only.as_ref().is_none_or(|o| o == k))
        .collect();

    // TIN_FUZZY=1: time the typo automaton alone (k = 1 and 2) on every query.
    if std::env::var("TIN_FUZZY").is_ok() {
        for k in [1u8, 2] {
            let mut v = Vec::new();
            for (_, q) in &queries {
                let t = q.trim().to_lowercase();
                let t0 = Instant::now();
                let mut n = 0;
                for s in &segments {
                    s.stream_terms(&tin_core::TermFilter::Fuzzy(&t, k), None, usize::MAX, |_| n += 1);
                }
                v.push(t0.elapsed());
            }
            v.sort();
            println!("fuzzy k={k}: p50 {:?} p99 {:?}", v[v.len() / 2], v[v.len() * 99 / 100]);
        }
        // The same k = 2 passes, resumed every 4 terms.
        let mut v = Vec::new();
        for (_, q) in &queries {
            let t = q.trim().to_lowercase();
            let t0 = Instant::now();
            for s in &segments {
                let mut after: Option<Vec<u8>> = None;
                loop {
                    after = s.stream_terms(&tin_core::TermFilter::Fuzzy(&t, 2), after.as_deref(), 4, |_| ());
                    if after.is_none() {
                        break;
                    }
                }
            }
            v.push(t0.elapsed());
        }
        v.sort();
        println!("fuzzy k=2 resumed every 4: p50 {:?} p99 {:?}", v[v.len() / 2], v[v.len() * 99 / 100]);
        return;
    }

    // (kind, tier) -> per-query (time, matches or candidates)
    let mut times: BTreeMap<(String, &str), Vec<(Duration, usize)>> = BTreeMap::new();
    let mut a = Analyzer::new();
    let mut out = Vec::new();
    for _ in 0..repeat {
        for (kind, q) in &queries {
            let t = q.trim().to_lowercase();
            let mut run = |tier: &'static str, query: String, out: &mut Vec<Tid>| {
                let plan = Plan::parse(&query, &mut a).unwrap();
                let t0 = Instant::now();
                out.clear();
                for s in &segments {
                    s.collect(&plan, out);
                }
                times.entry((kind.clone(), tier)).or_default().push((t0.elapsed(), out.len()));
                // Candidates (fragments) are narrowed by a recheck, as in Postgres.
                if plan.needs_recheck() {
                    out.retain(|t| plan.matches_text(&texts[t], &mut a2));
                }
                out.len()
            };
            let mut found = run("0 exact", t.clone(), &mut out).min(K);
            if !t.chars().all(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            if found < K {
                found += run("1 prefix", format!("{t}* -{t}"), &mut out).min(K - found);
            }
            if found > 0 || t.len() < 3 {
                continue;
            }
            if run("2 fragment", format!("*{t}*"), &mut out) > 0 {
                continue;
            }
            if run("3 typo1", format!("{t}~"), &mut out) > 0 || t.len() < 7 {
                continue;
            }
            run("4 typo2", format!("{t}~2"), &mut out);
        }
    }
    println!("| kind | tier | runs | p50 | p99 | mean | rows (mean) |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for ((kind, tier), mut v) in times {
        v.sort();
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize].0;
        let mean = v.iter().map(|x| x.0).sum::<Duration>() / v.len() as u32;
        let rows = v.iter().map(|x| x.1).sum::<usize>() as f64 / v.len() as f64;
        println!(
            "| {kind} | {tier} | {} | {:.3?} | {:.3?} | {:.3?} | {rows:.1} |",
            v.len(),
            p(0.5),
            p(0.99),
            mean
        );
    }
}
