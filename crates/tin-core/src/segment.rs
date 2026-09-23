//! Segments: self-contained inverted indexes over a contiguous range of heap
//! blocks.
//!
//! A segment is a term dictionary (an FST mapping term -> dictionary value)
//! plus a postings area. Because postings are keyed by tid, a segment that
//! covers blocks `[first_block, end_block)` only ever contains tids in that
//! range, so segments built in parallel over disjoint block ranges can be
//! queried back-to-back and yield results in heap order.

use std::collections::BTreeMap;

use fst::{Map, MapBuilder, Streamer};
use rustc_hash::FxHashMap;

use crate::cursor::{self, And, AndNot, Cursor, Empty, Or, SpaceTable};
use crate::postings::{Encoder, Encoding, PageDir, TermPostings, TAIL_PADDING};
use crate::query::Plan;
use crate::tid::Tid;
use crate::tokenize::Analyzer;

const MAGIC: &[u8; 4] = b"TIN\0";
const FORMAT_VERSION: u32 = 0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SegmentMeta {
    pub first_block: u32,
    /// Exclusive.
    pub end_block: u32,
    pub doc_count: u64,
    pub term_count: u64,
    pub posting_count: u64,
}

pub struct Segment {
    meta: SegmentMeta,
    /// Page directory: line-pointer count per block from `first_block`.
    widths: Vec<u16>,
    dict: Map<Vec<u8>>,
    /// Postings area, followed by `TAIL_PADDING` zero bytes.
    postings: Vec<u8>,
    /// Derived from `widths` at load.
    spaces: SpaceTable,
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
    last_tid: Option<Tid>,
    doc_count: u64,
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
            last_tid: None,
            doc_count: 0,
        }
    }

    /// Index one tuple. Tids must be strictly ascending and inside the
    /// segment's block range.
    pub fn add(&mut self, tid: Tid, text: &str) {
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

        let Self { analyzer, term_ids, terms, postings, .. } = self;
        analyzer.for_each_term(text, |term, _pos| {
            let id = match term_ids.get(term) {
                Some(&id) => id,
                None => {
                    let id = terms.len() as u32;
                    let boxed: Box<str> = term.into();
                    term_ids.insert(boxed.clone(), id);
                    terms.push(boxed);
                    postings.push(Vec::new());
                    id
                }
            };
            let list = &mut postings[id as usize];
            // Boolean postings: one entry per tuple however often the term repeats.
            if list.last() != Some(&tid) {
                list.push(tid);
            }
        });
    }

    pub fn finish(self) -> Segment {
        self.finish_with_stats(|_, _| ()).0
    }

    /// Like [`finish`](Self::finish), also returning postings size per
    /// (frequency class, encoding). `class_of(df, doc_count)` picks the class.
    pub fn finish_with_stats<C: Ord + Copy>(
        self,
        class_of: impl Fn(u64, u64) -> C,
    ) -> (Segment, BTreeMap<(C, Encoding), ClassStats>) {
        let mut order: Vec<u32> = (0..self.terms.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| self.terms[a as usize].cmp(&self.terms[b as usize]));

        let mut dict = MapBuilder::memory();
        let mut postings = Vec::new();
        let mut enc = Encoder::default();
        let dir = PageDir::new(self.first_block, &self.widths);
        let mut posting_count = 0u64;
        let mut stats: BTreeMap<(C, Encoding), ClassStats> = BTreeMap::new();
        for id in order {
            let tids = &self.postings[id as usize];
            posting_count += tids.len() as u64;
            let e = enc.encode_term(tids, dir, &mut postings);
            *stats.entry((class_of(tids.len() as u64, self.doc_count), e.encoding)).or_default() +=
                ClassStats { terms: 1, postings: tids.len() as u64, bytes: e.bytes as u64 };
            dict.insert(self.terms[id as usize].as_bytes(), e.value).expect("terms are unique and sorted");
        }
        let dict = Map::new(dict.into_inner().expect("fst build")).expect("fst load");
        postings.resize(postings.len() + TAIL_PADDING, 0);

        let spaces = SpaceTable::new(PageDir::new(self.first_block, &self.widths));
        let segment = Segment {
            spaces,
            meta: SegmentMeta {
                first_block: self.first_block,
                end_block: self.end_block,
                doc_count: self.doc_count,
                term_count: self.terms.len() as u64,
                posting_count,
            },
            widths: self.widths,
            dict,
            postings,
        };
        (segment, stats)
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
        self.dict_bytes() + self.postings_bytes() + self.page_dir_bytes()
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
        match plan {
            Plan::Term(t) => match self.term(t) {
                Some(tp) => tp.cursor(),
                None => Box::new(Empty),
            },
            Plan::And(cs) => {
                let mut kids = Vec::with_capacity(cs.len());
                for c in cs {
                    let k = self.cursor(c);
                    if k.cost() == 0 {
                        return Box::new(Empty);
                    }
                    kids.push(k);
                }
                Box::new(And::new(kids))
            }
            Plan::Or(cs) => {
                let kids: Vec<_> = cs.iter().map(|c| self.cursor(c)).filter(|k| k.cost() > 0).collect();
                match kids.len() {
                    0 => Box::new(Empty),
                    1 => kids.into_iter().next().unwrap(),
                    _ => Box::new(Or::new(kids)),
                }
            }
            Plan::AndNot(p, n) => {
                let pos = self.cursor(p);
                if pos.cost() == 0 {
                    return pos;
                }
                let neg = self.cursor(n);
                if neg.cost() == 0 {
                    return pos;
                }
                Box::new(AndNot::new(pos, neg))
            }
        }
    }

    pub fn search(&self, plan: &Plan, f: impl FnMut(Tid)) {
        cursor::for_each_tid(self.cursor(plan).as_mut(), &self.spaces, f);
    }

    /// Append every match to `out`, in tid order.
    pub fn collect(&self, plan: &Plan, out: &mut Vec<Tid>) {
        cursor::collect_tids(self.cursor(plan).as_mut(), &self.spaces, out);
    }

    pub fn count(&self, plan: &Plan) -> u64 {
        cursor::count(self.cursor(plan).as_mut(), &self.spaces)
    }

    // --- Serialization --------------------------------------------------------

    pub fn to_bytes(&self) -> Vec<u8> {
        let fst = self.dict.as_fst().as_bytes();
        let mut out = Vec::with_capacity(64 + self.widths.len() * 2 + fst.len() + self.postings.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.meta.first_block.to_le_bytes());
        out.extend_from_slice(&self.meta.end_block.to_le_bytes());
        for v in [self.meta.doc_count, self.meta.term_count, self.meta.posting_count] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(self.widths.len() as u64).to_le_bytes());
        for w in &self.widths {
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
        };
        let n_widths = r.u64()? as usize;
        let widths: Vec<u16> = r
            .take(n_widths.checked_mul(2).ok_or("bad page directory length")?)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
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
        let spaces = SpaceTable::new(PageDir::new(meta.first_block, &widths));
        Ok(Segment { meta, widths, dict, postings, spaces })
    }
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
