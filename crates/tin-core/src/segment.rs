//! Segments: self-contained inverted indexes over a range of heap blocks.
//!
//! A segment is a term dictionary (an FST mapping term -> dictionary value)
//! plus a postings area. Because postings are keyed by tid, a segment that
//! covers blocks `[first_block, end_block)` only ever contains tids in that
//! range, so segments built in parallel over disjoint block ranges can be
//! queried back-to-back and yield results in heap order.
//!
//! Every tuple a segment knows has a bit in its *tuple space*: blocks laid
//! end to end, `width(block)` bits each (the page directory). The segment
//! stores which of those bits are documents (`docs`); callers can pass a
//! liveness bitmap of the same shape to hide tuples deleted since.

use std::collections::BTreeMap;

use fst::automaton::Str;
use fst::{Automaton, IntoStreamer, Map, MapBuilder, Streamer};
use rustc_hash::FxHashMap;

use crate::cursor::{self, And, AndNot, Bits, Cursor, Empty, Or, SpaceTable};
use crate::pattern::{self, Osa, GRAM_MARK, GRAM_MIN_FRAGMENT};
use crate::postings::{Encoder, Encoding, PageDir, TermPostings, TAIL_PADDING};
use crate::query::Plan;
use crate::tid::Tid;
use crate::tokenize::Analyzer;

const MAGIC: &[u8; 4] = b"TIN\0";
const FORMAT_VERSION: u32 = 3;

/// Terms [`Segment::estimate`] reads from a prefix or typo expansion before
/// it stops counting (under-estimating, the safe side).
pub const ESTIMATE_TERMS: usize = 1000;

/// See [`Segment::fragment_terms`].
const DENSE_GRAM_FACTOR: u64 = 10;

/// Share of documents [`Segment::estimate`] assumes a fragment matches when
/// there are no grams to consult.
const FRAGMENT_GUESS: f64 = 1e-3;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SegmentMeta {
    pub first_block: u32,
    /// Exclusive.
    pub end_block: u32,
    pub doc_count: u64,
    pub term_count: u64,
    pub posting_count: u64,
    /// Terms' character 4-grams are indexed too (for fragment search; see
    /// [`pattern`](crate::pattern)).
    pub grams: bool,
}

pub struct Segment {
    meta: SegmentMeta,
    /// Page directory: line-pointer count per block from `first_block`.
    widths: Vec<u16>,
    dict: Map<Vec<u8>>,
    /// Postings area, followed by `TAIL_PADDING` zero bytes.
    postings: Vec<u8>,
    /// One bit per tuple-space position: set for every indexed tuple.
    docs: Vec<u64>,
    /// Derived from `widths` at load.
    spaces: SpaceTable,
    /// Derived: tuple-space bit of each block's first line pointer (+ total).
    block_start: Vec<u64>,
}

/// Accumulates documents (in ascending tid order) and encodes a [`Segment`].
pub struct SegmentBuilder {
    first_block: u32,
    end_block: u32,
    analyzer: Analyzer,
    term_ids: FxHashMap<Box<str>, u32>,
    terms: Vec<Box<str>>,
    postings: Vec<Vec<Tid>>,
    widths: Vec<u16>,
    docs: Vec<Tid>,
    last_tid: Option<Tid>,
    doc_count: u64,
    open_ended: bool,
    grams: bool,
    approx_bytes: usize,
}

impl SegmentBuilder {
    pub fn new(first_block: u32, end_block: u32) -> Self {
        assert!(first_block <= end_block);
        SegmentBuilder {
            first_block,
            end_block,
            analyzer: Analyzer::new(),
            term_ids: FxHashMap::default(),
            terms: Vec::new(),
            postings: Vec::new(),
            widths: Vec::new(),
            docs: Vec::new(),
            last_tid: None,
            doc_count: 0,
            open_ended: false,
            grams: false,
            approx_bytes: 0,
        }
    }

    /// A builder whose end is not known up front (streaming builds): the
    /// segment ends after the last block that received a tuple.
    pub fn open_ended(first_block: u32) -> Self {
        let mut b = Self::new(first_block, u32::MAX);
        b.open_ended = true;
        b
    }

    pub fn doc_count(&self) -> u64 {
        self.doc_count
    }

    /// Also index each term's character 4-grams, enabling fast
    /// `*fragment*` search (at the cost of a bigger index).
    pub fn with_grams(mut self, on: bool) -> Self {
        self.grams = on;
        self
    }

    /// Rough heap footprint, for memory-bounded builds. O(1): maintained as
    /// postings and terms are added (postings counted at 12 bytes for
    /// `Vec` growth slack).
    pub fn approx_bytes(&self) -> usize {
        self.approx_bytes + self.widths.len() * 2
    }

    /// Index one tuple. Tids must be strictly ascending and inside the
    /// segment's block range.
    pub fn add(&mut self, tid: Tid, text: &str) {
        self.begin_doc(tid);
        let grams = self.grams;
        let Self { analyzer, term_ids, terms, postings, approx_bytes, .. } = self;
        analyzer.for_each_term(text, |term, _pos| {
            Self::push_term(term_ids, terms, postings, approx_bytes, tid, term, grams);
        });
    }

    /// Index one tuple from already-analyzed terms (e.g. a pending-list
    /// record). Same ordering rules as [`add`](Self::add).
    pub fn add_terms<'t>(&mut self, tid: Tid, doc_terms: impl IntoIterator<Item = &'t str>) {
        self.begin_doc(tid);
        let grams = self.grams;
        let Self { term_ids, terms, postings, approx_bytes, .. } = self;
        for term in doc_terms {
            Self::push_term(term_ids, terms, postings, approx_bytes, tid, term, grams);
        }
    }

    fn begin_doc(&mut self, tid: Tid) {
        assert!(
            (self.first_block..self.end_block).contains(&tid.block),
            "{tid:?} outside segment blocks {}..{}",
            self.first_block,
            self.end_block
        );
        assert!(self.last_tid.is_none_or(|l| l < tid), "tids must be added in ascending order");
        self.last_tid = Some(tid);
        self.doc_count += 1;
        let page = (tid.block - self.first_block) as usize;
        if self.widths.len() <= page {
            self.widths.resize(page + 1, 0);
        }
        self.widths[page] = self.widths[page].max(tid.offset);
        self.docs.push(tid);
        self.approx_bytes += 8;
    }

    fn push_term(
        term_ids: &mut FxHashMap<Box<str>, u32>,
        terms: &mut Vec<Box<str>>,
        postings: &mut Vec<Vec<Tid>>,
        approx_bytes: &mut usize,
        tid: Tid,
        term: &str,
        grams: bool,
    ) {
        if grams {
            crate::pattern::grams(term, |g| {
                Self::push_term(term_ids, terms, postings, approx_bytes, tid, g, false)
            });
        }
        let id = match term_ids.get(term) {
            Some(&id) => id,
            None => {
                let id = terms.len() as u32;
                let boxed: Box<str> = term.into();
                term_ids.insert(boxed.clone(), id);
                terms.push(boxed);
                postings.push(Vec::new());
                // Two copies of the term + map entry + Vec header.
                *approx_bytes += term.len() * 2 + 72;
                id
            }
        };
        let list = &mut postings[id as usize];
        // Boolean postings: one entry per tuple however often the term repeats.
        if list.last() != Some(&tid) {
            list.push(tid);
            *approx_bytes += 12;
        }
    }

    pub fn finish(self) -> Segment {
        self.finish_with_stats(|_, _| ()).0
    }

    /// Like [`finish`](Self::finish), also returning postings size per
    /// (frequency class, encoding). `class_of(df, doc_count)` picks the class.
    pub fn finish_with_stats<C: Ord + Copy>(
        mut self,
        class_of: impl Fn(u64, u64) -> C,
    ) -> (Segment, BTreeMap<(C, Encoding), ClassStats>) {
        if self.open_ended {
            self.end_block = self.last_tid.map_or(self.first_block, |t| t.block + 1);
        }
        let mut order: Vec<u32> = (0..self.terms.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| self.terms[a as usize].cmp(&self.terms[b as usize]));

        let mut asm = Assembler::new(self.first_block, self.widths, self.grams);
        let mut stats: BTreeMap<(C, Encoding), ClassStats> = BTreeMap::new();
        for id in order {
            let tids = &self.postings[id as usize];
            let e = asm.push(self.terms[id as usize].as_bytes(), tids);
            *stats.entry((class_of(tids.len() as u64, self.doc_count), e.encoding)).or_default() +=
                ClassStats { terms: 1, postings: tids.len() as u64, bytes: e.bytes as u64 };
        }
        (asm.finish(self.end_block, &self.docs), stats)
    }
}

/// Encodes terms (pushed in sorted order) into a segment over a known page
/// directory. Shared by fresh builds and merges.
struct Assembler {
    first_block: u32,
    grams: bool,
    widths: Vec<u16>,
    dict: MapBuilder<Vec<u8>>,
    postings: Vec<u8>,
    enc: Encoder,
    posting_count: u64,
    term_count: u64,
}

impl Assembler {
    fn new(first_block: u32, widths: Vec<u16>, grams: bool) -> Self {
        Assembler {
            first_block,
            grams,
            widths,
            dict: MapBuilder::memory(),
            postings: Vec::new(),
            enc: Encoder::default(),
            posting_count: 0,
            term_count: 0,
        }
    }

    /// Add a term; terms must arrive in strictly ascending byte order and
    /// `tids` must be strictly ascending and non-empty.
    fn push(&mut self, term: &[u8], tids: &[Tid]) -> crate::postings::Encoded {
        let dir = PageDir::new(self.first_block, &self.widths);
        let e = self.enc.encode_term(tids, dir, &mut self.postings);
        self.dict.insert(term, e.value).expect("terms are unique and sorted");
        self.posting_count += tids.len() as u64;
        self.term_count += 1;
        e
    }

    /// `docs`: every indexed tid (any order).
    fn finish(self, end_block: u32, docs: &[Tid]) -> Segment {
        let dict = Map::new(self.dict.into_inner().expect("fst build")).expect("fst load");
        let mut postings = self.postings;
        postings.resize(postings.len() + TAIL_PADDING, 0);
        let block_start = block_starts(&self.widths);
        let mut doc_bits = vec![0u64; (*block_start.last().unwrap() as usize).div_ceil(64)];
        for t in docs {
            let bit = block_start[(t.block - self.first_block) as usize] as usize + t.offset_bit() as usize;
            doc_bits[bit >> 6] |= 1 << (bit & 63);
        }
        let spaces = SpaceTable::new(PageDir::new(self.first_block, &self.widths));
        Segment {
            spaces,
            block_start,
            docs: doc_bits,
            meta: SegmentMeta {
                first_block: self.first_block,
                end_block,
                doc_count: docs.len() as u64,
                term_count: self.term_count,
                posting_count: self.posting_count,
                grams: self.grams,
            },
            widths: self.widths,
            dict,
            postings,
        }
    }
}

impl Segment {
    /// Merge segments into one that holds only their live tuples. Each input
    /// comes with its liveness bitmap (`None` = all of its docs). Tuples may
    /// interleave across inputs but each tid must live in only one of them.
    /// Returns `None` if nothing is live.
    pub fn merge(inputs: &[(&Segment, Option<&[u64]>)]) -> Option<Segment> {
        let mut docs = Vec::new();
        for (seg, live) in inputs {
            seg.for_each_set_tid(live.unwrap_or(seg.docs()), |_, t| docs.push(t));
        }
        docs.sort_unstable();
        docs.dedup();
        let first_block = docs.first()?.block;
        let end_block = docs.last().unwrap().block + 1;
        let mut widths = vec![0u16; (end_block - first_block) as usize];
        for t in &docs {
            let w = &mut widths[(t.block - first_block) as usize];
            *w = (*w).max(t.offset);
        }

        let grams = inputs.iter().any(|(s, _)| s.meta.grams);
        let mut asm = Assembler::new(first_block, widths, grams);
        let mut op = fst::map::OpBuilder::new();
        for (seg, _) in inputs {
            op = op.add(&seg.dict);
        }
        let mut union = op.union();
        let mut tids = Vec::new();
        while let Some((term, values)) = union.next() {
            tids.clear();
            for v in values {
                let (seg, live) = inputs[v.index];
                let mut c = TermPostings::from_value(v.value, &seg.postings).cursor();
                cursor::for_each_tid(c.as_mut(), &seg.spaces, live, |t| tids.push(t));
            }
            if tids.is_empty() {
                continue; // every tuple with this term was deleted
            }
            tids.sort_unstable();
            asm.push(term, &tids);
        }
        Some(asm.finish(end_block, &docs))
    }
}

/// Size accounting for one frequency class of terms.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassStats {
    pub terms: u64,
    pub postings: u64,
    pub bytes: u64,
}

impl ClassStats {
    pub fn bits_per_posting(&self) -> f64 {
        if self.postings == 0 {
            0.0
        } else {
            self.bytes as f64 * 8.0 / self.postings as f64
        }
    }
}

impl std::ops::AddAssign for ClassStats {
    fn add_assign(&mut self, o: Self) {
        self.terms += o.terms;
        self.postings += o.postings;
        self.bytes += o.bytes;
    }
}

impl Segment {
    pub fn meta(&self) -> &SegmentMeta {
        &self.meta
    }

    pub fn dict_bytes(&self) -> usize {
        self.dict.as_fst().as_bytes().len()
    }

    pub fn postings_bytes(&self) -> usize {
        self.postings.len()
    }

    pub fn page_dir_bytes(&self) -> usize {
        self.widths.len() * 2
    }

    pub fn size_bytes(&self) -> usize {
        self.dict_bytes() + self.postings_bytes() + self.page_dir_bytes() + self.docs.len() * 8
    }

    pub fn term(&self, term: &str) -> Option<TermPostings<'_>> {
        self.dict.get(term).map(|v| TermPostings::from_value(v, &self.postings))
    }

    /// Every term with its document frequency in this segment, in term order.
    pub fn for_each_term(&self, mut f: impl FnMut(&[u8], TermPostings<'_>)) {
        let mut s = self.dict.stream();
        while let Some((term, v)) = s.next() {
            f(term, TermPostings::from_value(v, &self.postings));
        }
    }

    /// Build the cursor tree for `plan` against this segment.
    pub fn cursor<'a>(&'a self, plan: &Plan) -> Box<dyn Cursor + 'a> {
        self.cursor_in(plan, false)
    }

    /// `negated`: under a NOT, where every leaf must be exact (subtracting a
    /// candidate superset would lose real matches).
    fn cursor_in<'a>(&'a self, plan: &Plan, negated: bool) -> Box<dyn Cursor + 'a> {
        match plan {
            Plan::Term(t) => match self.term(t) {
                Some(tp) => tp.cursor(),
                None => Box::new(Empty),
            },
            Plan::Prefix(p) => self.expand(self.dict.search(Str::new(p).starts_with()).into_stream()),
            Plan::Fuzzy(q, k) => self.expand(self.dict.search(Osa::new(q.as_bytes(), *k)).into_stream()),
            Plan::Fragment(f) => match (self.meta.grams, f.chars().count()) {
                (true, GRAM_MIN_FRAGMENT) => {
                    // Exact: the tuples with a gram starting with it.
                    let p = pattern::fragment_gram_prefix(f);
                    self.expand_filtered(self.dict.search(Str::new(&p).starts_with()).into_stream(), |_| true)
                }
                (true, len) if len > GRAM_MIN_FRAGMENT && !negated => {
                    // Candidates: tuples with all of the fragment's grams
                    // (possibly spread over different terms); callers recheck.
                    self.cursor_in(&Plan::And(self.fragment_terms(f)), false)
                }
                _ => {
                    // Exact: scan the dictionary.
                    let f = f.as_str();
                    self.expand_filtered(self.dict.stream(), |t| !t.starts_with(GRAM_MARK) && t.contains(f))
                }
            },
            Plan::And(cs) => {
                let mut kids = Vec::with_capacity(cs.len());
                for c in cs {
                    let k = self.cursor_in(c, negated);
                    if k.cost() == 0 {
                        return Box::new(Empty);
                    }
                    kids.push(k);
                }
                if kids.len() == 1 {
                    return kids.pop().unwrap();
                }
                Box::new(And::new(kids))
            }
            Plan::Or(cs) => {
                let kids: Vec<_> =
                    cs.iter().map(|c| self.cursor_in(c, negated)).filter(|k| k.cost() > 0).collect();
                match kids.len() {
                    0 => Box::new(Empty),
                    1 => kids.into_iter().next().unwrap(),
                    _ => Box::new(Or::new(kids)),
                }
            }
            Plan::AndNot(p, n) => {
                let pos = self.cursor_in(p, negated);
                if pos.cost() == 0 {
                    return pos;
                }
                let neg = self.cursor_in(n, true);
                if neg.cost() == 0 {
                    return pos;
                }
                Box::new(AndNot::new(pos, neg))
            }
        }
    }

    /// The grams `*f*`'s candidates are intersected from: those at most
    /// [`DENSE_GRAM_FACTOR`] times as frequent as the rarest. Denser ones (an
    /// owner code in a third of all rows) cost more to intersect than the
    /// few candidates they would remove, which the recheck removes anyway.
    fn fragment_terms(&self, f: &str) -> Vec<Plan> {
        let mut grams = Vec::new();
        pattern::fragment_grams(f, |g| {
            grams.push((self.term(g).map_or(0, |tp| tp.doc_count()), g.to_owned()))
        });
        let rarest = grams.iter().map(|g| g.0).min().unwrap_or(0);
        grams
            .into_iter()
            .filter(|(df, _)| *df <= rarest.saturating_mul(DENSE_GRAM_FACTOR))
            .map(|(_, g)| Plan::Term(g))
            .collect()
    }

    /// Estimated number of documents matching `plan` (ignoring liveness),
    /// from dictionary statistics alone, for query planners: exact for single
    /// terms; summed over matching terms for prefixes and typos (a lower
    /// bound past [`ESTIMATE_TERMS`]); independence for fragments' grams
    /// and for AND / OR / NOT. When unsure it errs low: an index scan on a
    /// low estimate costs little, a sequential scan on a high one (planned
    /// for `LIMIT k`, expecting matches everywhere) reads the whole table.
    pub fn estimate(&self, plan: &Plan) -> f64 {
        self.fraction(plan) * self.meta.doc_count as f64
    }

    fn fraction(&self, plan: &Plan) -> f64 {
        let n = self.meta.doc_count.max(1) as f64;
        let df = |t: &str| self.term(t).map_or(0, |tp| tp.doc_count());
        let f = match plan {
            Plan::Term(t) => df(t) as f64 / n,
            Plan::Prefix(p) => {
                self.sum_doc_counts(self.dict.search(Str::new(p).starts_with()).into_stream(), false) / n
            }
            Plan::Fuzzy(q, k) => {
                self.sum_doc_counts(self.dict.search(Osa::new(q.as_bytes(), *k)).into_stream(), false) / n
            }
            Plan::Fragment(f) if self.meta.grams && f.chars().count() == GRAM_MIN_FRAGMENT => {
                let p = pattern::fragment_gram_prefix(f);
                self.sum_doc_counts(self.dict.search(Str::new(&p).starts_with()).into_stream(), true) / n
            }
            // As if the grams were independent. They overlap, so they aren't,
            // and this under-estimates: the safe side (see above).
            Plan::Fragment(f) if self.meta.grams && f.chars().count() > GRAM_MIN_FRAGMENT => {
                let mut p = 1.0;
                pattern::fragment_grams(f, |g| p *= df(g) as f64 / n);
                p
            }
            // Answering this means scanning the dictionary; don't do it twice.
            Plan::Fragment(_) => FRAGMENT_GUESS,
            Plan::And(cs) => cs.iter().map(|c| self.fraction(c)).product(),
            Plan::Or(cs) => 1.0 - cs.iter().map(|c| 1.0 - self.fraction(c)).product::<f64>(),
            Plan::AndNot(p, q) => self.fraction(p) * (1.0 - self.fraction(q)),
        };
        f.clamp(0.0, 1.0)
    }

    /// Summed document frequency of the stream's gram terms (`grams`) or
    /// other terms (fuzzy matching may meet grams, which don't count).
    fn sum_doc_counts<S>(&self, mut stream: S, grams: bool) -> f64
    where
        S: for<'s> Streamer<'s, Item = (&'s [u8], u64)>,
    {
        let (mut sum, mut terms) = (0u64, 0usize);
        while let Some((term, v)) = stream.next() {
            if (term.first() == Some(&(GRAM_MARK as u8))) != grams {
                continue;
            }
            sum += TermPostings::from_value(v, &self.postings).doc_count();
            terms += 1;
            if terms == ESTIMATE_TERMS {
                break;
            }
        }
        sum as f64
    }

    /// Union of the postings of every (non-gram) term in `stream`, as one
    /// tuple-space bitmap.
    fn expand<'a, S>(&'a self, stream: S) -> Box<dyn Cursor + 'a>
    where
        S: for<'s> Streamer<'s, Item = (&'s [u8], u64)>,
    {
        self.expand_filtered(stream, |t| !t.starts_with(GRAM_MARK))
    }

    /// Union of the postings of the terms in `stream` that `keep` accepts.
    fn expand_filtered<'a, S>(&'a self, mut stream: S, keep: impl Fn(&str) -> bool) -> Box<dyn Cursor + 'a>
    where
        S: for<'s> Streamer<'s, Item = (&'s [u8], u64)>,
    {
        let mut bits = vec![0u64; self.docs.len()];
        let mut any = false;
        while let Some((term, v)) = stream.next() {
            let Ok(term) = std::str::from_utf8(term) else { continue };
            if !keep(term) {
                continue;
            }
            any = true;
            match TermPostings::from_value(v, &self.postings) {
                TermPostings::Singleton(tid) => {
                    if let Some(b) = self.tid_bit(tid) {
                        bits[(b >> 6) as usize] |= 1 << (b & 63);
                    }
                }
                tp => {
                    let mut c = tp.cursor();
                    cursor::for_each_tid(c.as_mut(), &self.spaces, None, |t| {
                        if let Some(b) = self.tid_bit(t) {
                            bits[(b >> 6) as usize] |= 1 << (b & 63);
                        }
                    });
                }
            }
        }
        if !any {
            return Box::new(Empty);
        }
        Box::new(Bits::new(bits, &self.spaces))
    }

    pub fn search(&self, plan: &Plan, f: impl FnMut(Tid)) {
        self.search_live(plan, None, f)
    }

    /// Like [`search`](Self::search), skipping tuples whose bit in `live`
    /// (a bitmap shaped like [`docs`](Self::docs)) is clear.
    pub fn search_live(&self, plan: &Plan, live: Option<&[u64]>, f: impl FnMut(Tid)) {
        cursor::for_each_tid(self.cursor(plan).as_mut(), &self.spaces, live, f);
    }

    /// Append every match to `out`, in tid order.
    pub fn collect(&self, plan: &Plan, out: &mut Vec<Tid>) {
        self.collect_live(plan, None, out)
    }

    pub fn collect_live(&self, plan: &Plan, live: Option<&[u64]>, out: &mut Vec<Tid>) {
        cursor::collect_tids(self.cursor(plan).as_mut(), &self.spaces, live, out);
    }

    pub fn count(&self, plan: &Plan) -> u64 {
        self.count_live(plan, None)
    }

    pub fn count_live(&self, plan: &Plan, live: Option<&[u64]>) -> u64 {
        cursor::count(self.cursor(plan).as_mut(), &self.spaces, live)
    }

    // --- Tuple space -------------------------------------------------------

    /// Bitmap of indexed tuples, one bit per tuple-space position. The
    /// initial liveness bitmap.
    pub fn docs(&self) -> &[u64] {
        &self.docs
    }

    /// Number of tuple-space positions (bits in `docs`).
    pub fn tuple_bits(&self) -> u64 {
        *self.block_start.last().unwrap()
    }

    /// Tuple-space position of `tid`, if the segment's directory covers it.
    pub fn tid_bit(&self, tid: Tid) -> Option<u64> {
        let page = tid.block.checked_sub(self.meta.first_block)? as usize;
        let width = *self.widths.get(page)?;
        (tid.offset <= width).then(|| self.block_start[page] + tid.offset_bit() as u64)
    }

    /// Calls `f(bit, tid)` for every set bit of `bits` (shaped like `docs`).
    pub fn for_each_set_tid(&self, bits: &[u64], mut f: impl FnMut(u64, Tid)) {
        let mut page = 0usize;
        for (i, &word) in bits.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let bit = (i * 64) as u64 + w.trailing_zeros() as u64;
                while page + 1 < self.block_start.len() && self.block_start[page + 1] <= bit {
                    page += 1;
                }
                if page < self.widths.len() {
                    let tid = Tid::new(
                        self.meta.first_block + page as u32,
                        (bit - self.block_start[page]) as u16 + 1,
                    );
                    f(bit, tid);
                }
                w &= w - 1;
            }
        }
    }

    // --- Serialization --------------------------------------------------------

    pub fn to_bytes(&self) -> Vec<u8> {
        let fst = self.dict.as_fst().as_bytes();
        let mut out = Vec::with_capacity(
            80 + self.widths.len() * 2 + self.docs.len() * 8 + fst.len() + self.postings.len(),
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.meta.first_block.to_le_bytes());
        out.extend_from_slice(&self.meta.end_block.to_le_bytes());
        for v in [self.meta.doc_count, self.meta.term_count, self.meta.posting_count] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(self.meta.grams as u32).to_le_bytes());
        out.extend_from_slice(&(self.widths.len() as u64).to_le_bytes());
        for w in &self.widths {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&(self.docs.len() as u64).to_le_bytes());
        for w in &self.docs {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&(fst.len() as u64).to_le_bytes());
        out.extend_from_slice(fst);
        out.extend_from_slice(&(self.postings.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.postings);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Segment, String> {
        let mut r = Reader { b, pos: 0 };
        if r.take(4)? != MAGIC {
            return Err("bad magic".into());
        }
        let version = r.u32()?;
        if version != FORMAT_VERSION {
            return Err(format!("unsupported format version {version}"));
        }
        let meta = SegmentMeta {
            first_block: r.u32()?,
            end_block: r.u32()?,
            doc_count: r.u64()?,
            term_count: r.u64()?,
            posting_count: r.u64()?,
            grams: r.u32()? & 1 == 1,
        };
        let n_widths = r.u64()? as usize;
        let widths: Vec<u16> = r
            .take(n_widths.checked_mul(2).ok_or("bad page directory length")?)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let n_docs = r.u64()? as usize;
        let docs: Vec<u64> = r
            .take(n_docs.checked_mul(8).ok_or("bad docs length")?)?
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let fst_len = r.u64()? as usize;
        let dict = Map::new(r.take(fst_len)?.to_vec()).map_err(|e| e.to_string())?;
        let post_len = r.u64()? as usize;
        let postings = r.take(post_len)?.to_vec();
        if postings.len() < TAIL_PADDING || postings[postings.len() - TAIL_PADDING..].iter().any(|&b| b != 0)
        {
            return Err("postings area missing tail padding".into());
        }
        if r.pos != b.len() {
            return Err("trailing bytes".into());
        }
        let block_start = block_starts(&widths);
        if docs.len() != (*block_start.last().unwrap() as usize).div_ceil(64) {
            return Err("docs bitmap does not match the page directory".into());
        }
        let spaces = SpaceTable::new(PageDir::new(meta.first_block, &widths));
        Ok(Segment { meta, widths, dict, postings, docs, spaces, block_start })
    }
}

/// Prefix sums of `widths`: tuple-space bit of each block's first line
/// pointer, plus the total as the last element.
fn block_starts(widths: &[u16]) -> Vec<u64> {
    let mut v = Vec::with_capacity(widths.len() + 1);
    let mut acc = 0u64;
    v.push(0);
    for &w in widths {
        acc += w as u64;
        v.push(acc);
    }
    v
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or("truncated")?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}
