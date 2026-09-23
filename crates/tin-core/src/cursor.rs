//! Group-at-a-time query execution.
//!
//! A [`Cursor`] walks a set of tids one 256-page group at a time, in two
//! levels that mirror the storage:
//!
//! 1. **Pages.** Combine the children's [`PageBits`] (one 256-bit op) to find
//!    candidate pages. For AND this usually rules out whole groups, and most
//!    pages of the rest, before any offset data is touched.
//! 2. **Tuples.** For the candidate pages, children OR their exact matches
//!    into a bitmap over the group's *tuple space*: every line pointer of
//!    every page in the group, packed back to back (see [`GroupSpace`]).
//!    Boolean ops are then word-wise over that one bitmap.
//!
//! The tuple-space bitmap is our deviation from the TIN posts, which describe
//! one 512-bit register per page. On Stack Exchange data a page holds ~14
//! tuples, so a per-page register would be ~97% padding; packing the pages
//! lets one 512-bit op cover ~36 pages instead. Runs of consecutive pages are
//! stored contiguously on disk too, so dense terms fill it with straight bit
//! copies. The on-disk format is unchanged either way.
//!
//! `pages()` may over-approximate (a superset of the pages with matches); the
//! tuple bitmap is always exact.

use crate::bitmap::PageBits;
use crate::postings::PageDir;
use crate::tid::Tid;

/// Layout of one group's tuple space: page `p` owns bits
/// `start(p) .. start(p + 1)`, one per line pointer.
#[derive(Copy, Clone)]
pub struct GroupSpace<'a> {
    /// Cumulative widths.
    start: &'a [u32; 257],
}

static EMPTY_SPACE: [u32; 257] = [0; 257];

impl GroupSpace<'_> {
    /// First bit of `page` (`page` in `0..=256`; 256 = end of the group).
    #[inline]
    pub fn start(&self, page: usize) -> usize {
        self.start[page.min(256)] as usize
    }

    #[inline]
    pub fn bits(&self) -> usize {
        self.start[256] as usize
    }

    #[inline]
    pub fn words(&self) -> usize {
        self.bits().div_ceil(64)
    }

    /// Word range `lo..hi` covering every tuple of the pages in `mask`.
    #[inline]
    pub fn word_range(&self, mask: &PageBits) -> (usize, usize) {
        match (mask.first(), mask.last()) {
            (Some(a), Some(b)) => (self.start(a) >> 6, self.start(b + 1).div_ceil(64)),
            _ => (0, 0),
        }
    }

    /// Calls `f(page, offset_bit)` for every set bit of `bits`, ascending.
    /// Only pages in `pages` are visited; `bits` must be zero elsewhere.
    #[inline]
    pub fn for_each_tuple(&self, bits: &[u64], pages: &PageBits, mut f: impl FnMut(u8, u16)) {
        pages.for_each(|p| {
            let (lo, hi) = (self.start(p), self.start(p + 1));
            let mut base = lo;
            while base < hi {
                let n = (hi - base).min(64);
                let (w, sh) = (base >> 6, base & 63);
                let mut v = bits[w] >> sh;
                if sh != 0 && sh + n > 64 {
                    v |= bits[w + 1] << (64 - sh);
                }
                if n < 64 {
                    v &= (1u64 << n) - 1;
                }
                let off = (base - lo) as u16;
                while v != 0 {
                    f(p as u8, off + v.trailing_zeros() as u16);
                    v &= v - 1;
                }
                base += n;
            }
        });
    }
}

/// Tuple-space layouts for every group a segment covers, precomputed from
/// its page directory (4 bytes per heap page; derived, not stored on disk).
pub struct SpaceTable {
    first_group: u32,
    starts: Vec<u32>,
}

impl SpaceTable {
    pub fn new(dir: PageDir<'_>) -> Self {
        let blocks = dir.blocks();
        if blocks.is_empty() {
            return SpaceTable { first_group: 0, starts: Vec::new() };
        }
        let first_group = blocks.start >> 8;
        let last_group = (blocks.end - 1) >> 8;
        let mut starts = Vec::with_capacity((last_group - first_group + 1) as usize * 257);
        for g in first_group..=last_group {
            let mut acc = 0u32;
            starts.push(0);
            for p in 0..256 {
                acc += dir.width((g << 8) | p) as u32;
                starts.push(acc);
            }
        }
        SpaceTable { first_group, starts }
    }

    pub fn group(&self, g: u32) -> GroupSpace<'_> {
        let i = g.wrapping_sub(self.first_group) as usize * 257;
        match self.starts.get(i..i + 257) {
            Some(start) => GroupSpace { start: start.try_into().unwrap() },
            None => GroupSpace { start: &EMPTY_SPACE },
        }
    }

    pub fn bytes(&self) -> usize {
        self.starts.len() * 4
    }
}

pub trait Cursor {
    /// Move to the first group `>= target` that may contain matches and
    /// return it, or `None` when exhausted. Idempotent: if the cursor already
    /// sits on a group `>= target` it stays there.
    fn seek(&mut self, target: u32) -> Option<u32>;

    /// Candidate pages in the current group (superset of the real matches).
    fn pages(&mut self) -> PageBits;

    /// OR this cursor's exact matches on pages in `mask` into `out`, a bitmap
    /// over the current group's tuple space (`space.words()` long).
    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]);

    /// Upper bound on the number of matches; AND evaluates cheapest first.
    fn cost(&self) -> u64;
}

/// Matches nothing (a term missing from the segment).
pub struct Empty;

impl Cursor for Empty {
    fn seek(&mut self, _: u32) -> Option<u32> {
        None
    }
    fn pages(&mut self) -> PageBits {
        PageBits::ZERO
    }
    fn or_into(&mut self, _: &PageBits, _: &GroupSpace<'_>, _: &mut [u64]) {}
    fn cost(&self) -> u64 {
        0
    }
}

/// Reusable scratch bitmap.
#[derive(Default)]
struct Scratch(Vec<u64>);

impl Scratch {
    /// A `words`-long buffer whose words `lo..hi` are zero. Words outside
    /// that range hold garbage and must not be read.
    fn zeroed(&mut self, words: usize, lo: usize, hi: usize) -> &mut [u64] {
        if self.0.len() < words {
            self.0.resize(words, 0);
        }
        self.0[lo..hi].fill(0);
        &mut self.0[..words]
    }
}

/// Intersection. Leapfrogs on group ids, then ANDs page bitmaps and skips any
/// group whose page intersection is empty without touching tuple data.
pub struct And<'a> {
    children: Vec<Box<dyn Cursor + 'a>>,
    cur: Option<u32>,
    pages: PageBits,
    acc: Scratch,
    tmp: Scratch,
}

impl<'a> And<'a> {
    pub fn new(mut children: Vec<Box<dyn Cursor + 'a>>) -> Self {
        assert!(!children.is_empty());
        children.sort_by_key(|c| c.cost());
        And { children, cur: None, pages: PageBits::ZERO, acc: Scratch::default(), tmp: Scratch::default() }
    }
}

impl Cursor for And<'_> {
    fn seek(&mut self, target: u32) -> Option<u32> {
        if let Some(g) = self.cur {
            if g >= target {
                return Some(g);
            }
        }
        let mut g = target;
        'outer: loop {
            for c in self.children.iter_mut() {
                match c.seek(g) {
                    None => {
                        self.cur = None;
                        return None;
                    }
                    Some(cg) if cg > g => {
                        g = cg;
                        continue 'outer;
                    }
                    Some(_) => {}
                }
            }
            // Every child is on group g: prune at page level.
            let mut pages = self.children[0].pages();
            for c in &mut self.children[1..] {
                pages &= c.pages();
                if pages.is_zero() {
                    break;
                }
            }
            if pages.is_zero() {
                g += 1;
                continue;
            }
            self.cur = Some(g);
            self.pages = pages;
            return Some(g);
        }
    }

    fn pages(&mut self) -> PageBits {
        self.pages
    }

    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]) {
        let mask = *mask & self.pages;
        if mask.is_zero() {
            return;
        }
        let (lo, hi) = space.word_range(&mask);
        let acc = self.acc.zeroed(space.words(), lo, hi);
        self.children[0].or_into(&mask, space, acc);
        for c in &mut self.children[1..] {
            let tmp = self.tmp.zeroed(space.words(), lo, hi);
            c.or_into(&mask, space, tmp);
            let mut any = 0;
            for (a, t) in acc[lo..hi].iter_mut().zip(&tmp[lo..hi]) {
                *a &= t;
                any |= *a;
            }
            if any == 0 {
                return;
            }
        }
        for (o, a) in out[lo..hi].iter_mut().zip(&acc[lo..hi]) {
            *o |= a;
        }
    }

    fn cost(&self) -> u64 {
        self.children[0].cost()
    }
}

/// Union. The current group is the minimum over children; only children
/// sitting on it contribute.
pub struct Or<'a> {
    children: Vec<Box<dyn Cursor + 'a>>,
    /// Each child's current group (None = exhausted).
    at: Vec<Option<u32>>,
    started: bool,
    cur: Option<u32>,
    /// Page bitmap of each child for the current group (zero if inactive).
    child_pages: Vec<PageBits>,
}

impl<'a> Or<'a> {
    pub fn new(children: Vec<Box<dyn Cursor + 'a>>) -> Self {
        assert!(!children.is_empty());
        let n = children.len();
        Or { children, at: vec![None; n], started: false, cur: None, child_pages: vec![PageBits::ZERO; n] }
    }
}

impl Cursor for Or<'_> {
    fn seek(&mut self, target: u32) -> Option<u32> {
        if let Some(g) = self.cur {
            if g >= target {
                return Some(g);
            }
        }
        let mut min = None;
        for (i, c) in self.children.iter_mut().enumerate() {
            // Exhausted children stay exhausted; don't re-seek them.
            if self.started && self.at[i].is_none() {
                continue;
            }
            self.at[i] = c.seek(target);
            min = match (min, self.at[i]) {
                (None, x) => x,
                (Some(m), Some(x)) => Some(m.min(x)),
                (m, None) => m,
            };
        }
        self.started = true;
        self.cur = min;
        if min.is_some() {
            for (i, c) in self.children.iter_mut().enumerate() {
                self.child_pages[i] = if self.at[i] == min { c.pages() } else { PageBits::ZERO };
            }
        }
        min
    }

    fn pages(&mut self) -> PageBits {
        self.child_pages.iter().fold(PageBits::ZERO, |acc, &p| acc | p)
    }

    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]) {
        for (c, pages) in self.children.iter_mut().zip(&self.child_pages) {
            let m = *mask & *pages;
            if !m.is_zero() {
                c.or_into(&m, space, out);
            }
        }
    }

    fn cost(&self) -> u64 {
        self.children.iter().map(|c| c.cost()).sum()
    }
}

/// `positive AND NOT negative`.
pub struct AndNot<'a> {
    pos: Box<dyn Cursor + 'a>,
    neg: Box<dyn Cursor + 'a>,
    neg_pages: PageBits,
    p: Scratch,
    n: Scratch,
}

impl<'a> AndNot<'a> {
    pub fn new(pos: Box<dyn Cursor + 'a>, neg: Box<dyn Cursor + 'a>) -> Self {
        AndNot { pos, neg, neg_pages: PageBits::ZERO, p: Scratch::default(), n: Scratch::default() }
    }
}

impl Cursor for AndNot<'_> {
    fn seek(&mut self, target: u32) -> Option<u32> {
        let g = self.pos.seek(target)?;
        self.neg_pages = if self.neg.seek(g) == Some(g) { self.neg.pages() } else { PageBits::ZERO };
        Some(g)
    }

    fn pages(&mut self) -> PageBits {
        // Can't subtract pages: the negative side may cover only some tuples.
        self.pos.pages()
    }

    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]) {
        let neg_mask = *mask & self.neg_pages;
        if neg_mask.is_zero() {
            return self.pos.or_into(mask, space, out);
        }
        let (lo, hi) = space.word_range(mask);
        let p = self.p.zeroed(space.words(), lo, hi);
        self.pos.or_into(mask, space, p);
        let n = self.n.zeroed(space.words(), lo, hi);
        self.neg.or_into(&neg_mask, space, n);
        for ((o, p), n) in out[lo..hi].iter_mut().zip(&p[lo..hi]).zip(&n[lo..hi]) {
            *o |= p & !n;
        }
    }

    fn cost(&self) -> u64 {
        self.pos.cost()
    }
}

/// Evaluate `c` group by group, calling `f(group, space, pages, bits)` with
/// each candidate group's exact tuple bitmap (`bits` is zero outside `pages`).
fn drive(
    c: &mut dyn Cursor,
    spaces: &SpaceTable,
    mut f: impl FnMut(u32, &GroupSpace<'_>, &PageBits, &[u64]),
) {
    let mut buf: Vec<u64> = Vec::new();
    let mut g = 0;
    while let Some(cur) = c.seek(g) {
        let space = spaces.group(cur);
        let pages = c.pages();
        buf.clear();
        buf.resize(space.words(), 0);
        c.or_into(&pages, &space, &mut buf);
        f(cur, &space, &pages, &buf);
        match cur.checked_add(1) {
            Some(next) => g = next,
            None => break,
        }
    }
}

/// Drive a cursor to completion, calling `f` for every match in tid order.
pub fn for_each_tid(c: &mut dyn Cursor, spaces: &SpaceTable, mut f: impl FnMut(Tid)) {
    drive(c, spaces, |g, space, pages, bits| {
        space.for_each_tuple(bits, pages, |page, bit| f(Tid::from_parts(g, page, bit)));
    });
}

/// Append every match to `out`, in tid order.
pub fn collect_tids(c: &mut dyn Cursor, spaces: &SpaceTable, out: &mut Vec<Tid>) {
    drive(c, spaces, |g, space, pages, bits| {
        out.reserve(bits.iter().map(|w| w.count_ones() as usize).sum());
        space.for_each_tuple(bits, pages, |page, bit| out.push(Tid::from_parts(g, page, bit)));
    });
}

/// Count matches without materializing tids (POPCNT over tuple bitmaps).
pub fn count(c: &mut dyn Cursor, spaces: &SpaceTable) -> u64 {
    let mut n = 0u64;
    drive(c, spaces, |_, _, _, bits| n += bits.iter().map(|w| w.count_ones() as u64).sum::<u64>());
    n
}
