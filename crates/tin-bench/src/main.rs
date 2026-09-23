//! Phase 0 benchmark.
//!
//! 1. Load a one-document-per-line corpus.
//! 2. Lay the documents out as Postgres would (8 KB heap pages, tuple headers,
//!    line pointers, TOAST for long values) to get realistic ctids.
//! 3. Build the index with N parallel segments; report build time, size and
//!    bits per posting by term-frequency class.
//! 4. Generate a seeded query set (conjunction / disjunction / mixed /
//!    negation), run it on TIN and on a textbook baseline (uncompressed sorted
//!    posting arrays), check every answer matches, report latency + QPS.
//!
//! Usage:
//!   tin-bench <corpus.txt> [--segments N] [--threads N] [--queries-per-kind N]
//!                          [--seed N] [--max-docs N] [--out report.md]

mod baseline;
mod heap;
mod queries;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use tin_core::postings::Encoding;
use tin_core::segment::ClassStats;
use tin_core::{Index, Plan, Tid};

use crate::baseline::Baseline;
use crate::queries::{Kind, QuerySet};

struct Args {
    corpus: String,
    segments: usize,
    threads: usize,
    per_kind: usize,
    seed: u64,
    max_docs: usize,
    out: Option<String>,
}

fn parse_args() -> Args {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut a = Args {
        corpus: String::new(),
        segments: cpus,
        threads: cpus,
        per_kind: 1000,
        seed: 42,
        max_docs: usize::MAX,
        out: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--segments" => a.segments = val().parse().unwrap(),
            "--threads" => a.threads = val().parse().unwrap(),
            "--queries-per-kind" => a.per_kind = val().parse().unwrap(),
            "--seed" => a.seed = val().parse().unwrap(),
            "--max-docs" => a.max_docs = val().parse().unwrap(),
            "--out" => a.out = Some(val()),
            s if !s.starts_with("--") && a.corpus.is_empty() => a.corpus = s.to_owned(),
            s => panic!("unknown argument {s}"),
        }
    }
    assert!(!a.corpus.is_empty(), "usage: tin-bench <corpus.txt> [options]");
    a
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// In >= 1% of tuples.
    High,
    /// In >= 0.01% of tuples.
    Medium,
    /// Fewer.
    Rare,
}

fn class_of(df: u64, docs: u64) -> Class {
    if df * 100 >= docs {
        Class::High
    } else if df * 10_000 >= docs {
        Class::Medium
    } else {
        Class::Rare
    }
}

type Engine<'a> = &'a (dyn Fn(&Plan) -> u64 + Sync);

fn main() {
    let args = parse_args();
    let mut report = String::new();
    macro_rules! out {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            writeln!(report, "{line}").unwrap();
        }};
    }

    // --- Load + lay out on heap pages ----------------------------------------
    let t = Instant::now();
    let text = std::fs::read_to_string(&args.corpus).expect("read corpus");
    let lines: Vec<&str> = text.lines().take(args.max_docs).collect();
    let corpus_bytes: usize = lines.iter().map(|l| l.len()).sum();
    let layout = heap::layout(&lines);
    let docs: Vec<(Tid, &str)> = layout.tids.iter().copied().zip(lines.iter().copied()).collect();
    eprintln!("loaded {} docs in {:.1?}", docs.len(), t.elapsed());

    out!("## Corpus\n");
    out!("| | |\n|---|---|");
    out!("| File | `{}` |", args.corpus.rsplit('/').next().unwrap());
    out!("| Documents | {} |", fmt_n(docs.len() as u64));
    out!("| Text | {} |", fmt_bytes(corpus_bytes as u64));
    out!(
        "| Simulated heap | {} pages of 8 KB, {:.1} tuples/page, {} values TOASTed |",
        fmt_n(layout.pages as u64),
        docs.len() as f64 / layout.pages as f64,
        fmt_n(layout.toasted as u64)
    );
    out!(
        "| Machine | {} hardware threads; {} used |",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        args.threads
    );

    // --- Build ---------------------------------------------------------------
    let pool = rayon::ThreadPoolBuilder::new().num_threads(args.threads).build().unwrap();
    let t = Instant::now();
    let (index, stats) = pool.install(|| Index::build_with_stats(&docs, args.segments, class_of));
    let build = t.elapsed();
    let postings: u64 = index.segments().iter().map(|s| s.meta().posting_count).sum();
    let dict: usize = index.segments().iter().map(|s| s.dict_bytes()).sum();
    let post: usize = index.segments().iter().map(|s| s.postings_bytes()).sum();
    let pdir: usize = index.segments().iter().map(|s| s.page_dir_bytes()).sum();
    let total = dict + post + pdir;

    out!("\n## Build\n");
    out!("| | |\n|---|---|");
    out!("| Segments | {} (built in parallel over disjoint block ranges) |", index.segments().len());
    out!(
        "| Build time | {:.2?} ({} docs/s, {}/s of text) |",
        build,
        fmt_n((docs.len() as f64 / build.as_secs_f64()) as u64),
        fmt_bytes((corpus_bytes as f64 / build.as_secs_f64()) as u64)
    );
    out!("| Postings (tuple, term) pairs | {} |", fmt_n(postings));
    out!(
        "| Index size | {} = dictionary {} + postings {} + page directory {} |",
        fmt_bytes(total as u64),
        fmt_bytes(dict as u64),
        fmt_bytes(post as u64),
        fmt_bytes(pdir as u64)
    );
    out!("| Index / text | {:.1}% |", 100.0 * total as f64 / corpus_bytes as f64);
    out!("| Bits per posting, all terms | {:.2} |", post as f64 * 8.0 / postings as f64);

    out!("\n### Bits per posting by term frequency\n");
    out!("High = term in >= 1% of tuples, Medium = >= 0.01%, Rare = fewer.\n");
    out!("| Class | Encoding | Terms | Postings | Bytes | Bits/posting |");
    out!("|---|---|---:|---:|---:|---:|");
    let mut by_class = std::collections::BTreeMap::<Class, ClassStats>::new();
    for (&(class, enc), s) in &stats {
        *by_class.entry(class).or_default() += *s;
        let enc = match enc {
            Encoding::Singleton => "singleton (inline in dictionary)",
            Encoding::Sparse => "sparse gap list",
            Encoding::Groups => "two-level bitmap",
        };
        out!(
            "| {class:?} | {enc} | {} | {} | {} | {:.2} |",
            fmt_n(s.terms),
            fmt_n(s.postings),
            fmt_bytes(s.bytes),
            s.bits_per_posting()
        );
    }
    for (class, s) in &by_class {
        out!(
            "| **{class:?} total** | | {} | {} | {} | **{:.2}** |",
            fmt_n(s.terms),
            fmt_n(s.postings),
            fmt_bytes(s.bytes),
            s.bits_per_posting()
        );
    }

    // `TIN_DUMP_QUERIES=path`: write the query set (kind<TAB>query per line)
    // for running the same mix through Postgres, then exit.
    if let Ok(path) = std::env::var("TIN_DUMP_QUERIES") {
        let qs = QuerySet::generate(&index, args.per_kind, args.seed);
        let lines: Vec<String> = qs.all().iter().map(|q| format!("{}\t{}", q.kind.name(), q.text)).collect();
        std::fs::write(&path, lines.join("\n") + "\n").expect("write queries");
        eprintln!("wrote {} queries to {path}", lines.len());
        return;
    }

    // Profiling hook: `TIN_PROFILE=conjunction` runs just that kind on TIN
    // (single thread) and exits, for use under callgrind/perf. Add
    // `TIN_PROFILE_MODE=tids` to materialize tids instead of counting.
    if let Ok(kind) = std::env::var("TIN_PROFILE") {
        let qs = QuerySet::generate(&index, args.per_kind, args.seed);
        let kind = Kind::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(&kind)).expect("kind");
        let tids = std::env::var("TIN_PROFILE_MODE").is_ok_and(|m| m == "tids");
        let n: u64 = qs
            .of(kind)
            .map(|q| if tids { index.search_vec(&q.plan).len() as u64 } else { index.count(&q.plan) })
            .sum();
        eprintln!("profiled {} {} queries, {n} matches", args.per_kind, kind.name());
        return;
    }

    // --- Baseline -------------------------------------------------------------
    let t = Instant::now();
    let baseline = pool.install(|| Baseline::build(&docs, args.segments));
    eprintln!("baseline built in {:.1?}", t.elapsed());

    // --- Queries --------------------------------------------------------------
    let qs = QuerySet::generate(&index, args.per_kind, args.seed);
    out!("\n## Queries\n");
    out!(
        "{} queries per kind ({} total), seed {}. Terms drawn from the {} terms in >= {} tuples \
         (30% from the 500 most frequent).\n",
        args.per_kind,
        qs.all().len(),
        args.seed,
        fmt_n(qs.pool_size as u64),
        fmt_n(queries::MIN_DF)
    );
    for kind in Kind::ALL {
        let ex: Vec<String> = qs.of(kind).take(2).map(|q| format!("`{}`", q.text)).collect();
        out!("- **{}**: e.g. {}", kind.name(), ex.join(", "));
    }

    // Correctness first: TIN == baseline for every query.
    let mismatches: usize = pool.install(|| {
        qs.all()
            .par_iter()
            .filter(|q| {
                let a = index.search_vec(&q.plan);
                let b = baseline.eval(&q.plan);
                a.len() != b.len() || a.iter().zip(b.iter()).any(|(t, &k)| t.key() != k)
            })
            .count()
    });
    out!(
        "\nCorrectness: **{} / {} queries return exactly the baseline's tids**.",
        qs.all().len() - mismatches,
        qs.all().len()
    );
    assert_eq!(mismatches, 0, "TIN disagrees with baseline");

    out!("\n## Speed\n");
    out!(
        "Latency is single-threaded, one query at a time; QPS runs the whole set on {} threads. \
         Baseline = uncompressed sorted `u64` posting arrays with merge/galloping set ops \
         ({} of postings vs TIN's {}) — a generous textbook inverted index held fully in RAM.\n",
        args.threads,
        fmt_bytes(baseline.bytes() as u64),
        fmt_bytes(post as u64)
    );
    out!("| Query kind | Mode | Engine | Avg matches | p50 | p99 | QPS |");
    out!("|---|---|---|---:|---:|---:|---:|");
    let tin_count: Engine = &|p| index.count(p);
    let tin_tids: Engine = &|p| index.search_vec(p).len() as u64;
    let base: Engine = &|p| baseline.eval(p).len() as u64;
    for kind in Kind::ALL {
        let plans: Vec<&Plan> = qs.of(kind).map(|q| &q.plan).collect();
        let avg = plans.iter().map(|p| index.count(p)).sum::<u64>() as f64 / plans.len() as f64;
        for (mode, engine, f) in
            [("COUNT(*)", "TIN", tin_count), ("all tids", "TIN", tin_tids), ("either", "baseline", base)]
        {
            let lat = latencies(&plans, f);
            let qps = pool.install(|| throughput(&plans, f));
            out!(
                "| {} | {mode} | {engine} | {} | {} | {} | {} |",
                kind.name(),
                fmt_n(avg as u64),
                fmt_dur(pct(&lat, 50.0)),
                fmt_dur(pct(&lat, 99.0)),
                fmt_n(qps as u64)
            );
        }
    }

    if let Some(path) = args.out {
        std::fs::write(&path, report).expect("write report");
        eprintln!("wrote {path}");
    }
}

fn latencies(plans: &[&Plan], f: Engine) -> Vec<Duration> {
    let mut v: Vec<Duration> = plans
        .iter()
        .map(|p| {
            let t = Instant::now();
            std::hint::black_box(f(p));
            t.elapsed()
        })
        .collect();
    v.sort();
    v
}

/// Run the whole set in parallel repeatedly for at least 2 s.
fn throughput(plans: &[&Plan], f: Engine) -> f64 {
    let mut rounds = 0;
    let t = Instant::now();
    while rounds == 0 || t.elapsed() < Duration::from_secs(2) {
        plans.par_iter().for_each(|p| {
            std::hint::black_box(f(p));
        });
        rounds += 1;
    }
    (rounds * plans.len()) as f64 / t.elapsed().as_secs_f64()
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize]
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{us:.0} µs")
    } else {
        format!("{:.2} ms", us / 1000.0)
    }
}

fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.1} KB", b / 1e3)
    }
}
