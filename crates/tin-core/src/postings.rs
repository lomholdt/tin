//! Postings: the on-disk encodings of one term's set of tids, and the cursors
//! that read them.
//!
//! Every term gets one of three encodings (the smallest, with a bias towards
//! bitmaps; see [`GROUPS_BIAS_NUM`]):
//!
//! * **Singleton** — a term in exactly one tuple stores its tid inline in the
//!   dictionary value. No postings bytes at all ("single-occurrence terms
//!   bypass bitmap storage entirely").
//! * **Sparse** — rare and mid-frequency terms: the tids as ascending
//!   [`Tid::key`]s, gap-coded with varints. Gaps between rare tids are large,
//!   so this lands around 20–30 bits per posting, which is the "rare terms
//!   approach 25 bits" regime. Lists longer than [`SKIP_BLOCK`] are cut into
//!   blocks behind a skip table so AND can jump over them.
//! * **Groups** — the two-level bitmap. For each 256-page group that contains
//!   the term: a page set (list of page bytes, or a 256-bit [`PageBits`]),
//!   then, for each of those pages, an offset bitmap exactly as wide as that
//!   page's line-pointer count. The widths come from the segment's shared
//!   [`PageDir`], so no per-page headers are stored: a page holding 14 tuples
//!   costs a term 14 bits. Dense terms approach 1 bit per posting.
//!
//! Byte layout (integers are LEB128 varints unless noted):
//!
//! ```text
//! term entry  := doc_count body
//! sparse body := key_0 (key_i - key_{i-1})*            -- doc_count <= SKIP_BLOCK
//!              | skip* block*                          -- otherwise
//! skip        := first_key_delta block_len             -- one per SKIP_BLOCK keys
//! block       := (key_i - key_{i-1})*                  -- first key lives in the skip
//! groups body := n_groups group*
//! group       := group_delta body_len group_body        -- body_len lets seek skip
//! group_body  := u8(n_pages - 1)
//!                ( u8 page * n_pages | 32-byte PageBits )  -- list if n_pages <= 31
//!                bitstream                                 -- LSB-first, byte padded:
//!                                                          -- width(page) bits per page,
//!                                                          -- ascending page order
//! ```
//!
//! Readers may load up to 16 bytes past any bitstream position, so the
//! postings area must end with [`TAIL_PADDING`] zero bytes.

use crate::bitmap::{OffsetBits, PageBits};
use crate::cursor::{Cursor, GroupSpace};
use crate::tid::Tid;
use crate::varint;

/// Page sets with at most this many pages are stored as a byte list; above it
/// the 32-byte bitmap is never larger.
pub const PAGE_LIST_MAX: usize = 31;

/// For terms with more than [`SKIP_BLOCK`] postings the bitmap encoding is
/// chosen unless the sparse list is smaller than `groups_bytes * DEN / NUM`,
/// i.e. bitmaps may be up to NUM/DEN x larger.
pub const GROUPS_BIAS_NUM: usize = 3;
pub const GROUPS_BIAS_DEN: usize = 2;

/// Sparse lists longer than this get a skip table, one entry per block of
/// this many keys.
pub const SKIP_BLOCK: usize = 64;

/// Zero bytes required after the last postings byte (see module docs).
pub const TAIL_PADDING: usize = 16;

// Dictionary values: 2-bit tag + 62-bit payload (postings offset or tid key).
const TAG_SHIFT: u32 = 62;
const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;
const TAG_GROUPS: u64 = 0;
const TAG_SPARSE: u64 = 1;
const TAG_SINGLETON: u64 = 2;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Encoding {
    Singleton,
    Sparse,
    Groups,
}

/// A segment's page directory: the line-pointer count (highest offset in
/// use) of every heap page from `first_block` on. It sets the width of every
/// offset bitmap on that page, for every term.
#[derive(Copy, Clone, Debug)]
pub struct PageDir<'a> {
    first_block: u32,
    widths: &'a [u16],
}

impl<'a> PageDir<'a> {
    pub fn new(first_block: u32, widths: &'a [u16]) -> Self {
        PageDir { first_block, widths }
    }

    /// Blocks the directory covers.
    pub fn blocks(&self) -> std::ops::Range<u32> {
        self.first_block..self.first_block + self.widths.len() as u32
    }

    /// Offset-bitmap width, in bits, for `block` (0 outside the directory).
    #[inline]
    pub fn width(&self, block: u32) -> usize {
        self.widths.get(block.wrapping_sub(self.first_block) as usize).map_or(0, |&w| w as usize)
    }
}

/// Reusable scratch space for [`Encoder::encode_term`].
#[derive(Default)]
pub struct Encoder {
    sparse: Vec<u8>,
    blocks: Vec<u8>,
    groups: Vec<u8>,
    group_body: Vec<u8>,
}

/// Result of encoding one term.
pub struct Encoded {
    /// Value to store in the term dictionary.
    pub value: u64,
    pub encoding: Encoding,
    /// Bytes appended to the postings area (0 for singletons).
    pub bytes: usize,
}

impl Encoder {
    /// Append the postings for `tids` (strictly ascending, non-empty) to `out`.
    /// Every tid's offset must fit its page's width in `dir`.
    pub fn encode_term(&mut self, tids: &[Tid], dir: PageDir<'_>, out: &mut Vec<u8>) -> Encoded {
        assert!(!tids.is_empty());
        debug_assert!(tids.windows(2).all(|w| w[0] < w[1]), "tids must be strictly ascending");

        if tids.len() == 1 {
            return Encoded {
                value: (TAG_SINGLETON << TAG_SHIFT) | tids[0].key(),
                encoding: Encoding::Singleton,
                bytes: 0,
            };
        }

        self.encode_sparse(tids);
        self.encode_groups(tids, dir);
        // Prefer the bitmap even when somewhat larger: AND/NOT prune whole
        // pages from it without decoding postings, while a sparse list must
        // be decoded posting by posting. (Measured on Stack Exchange data.)
        // Short lists gain nothing from pruning, so they just take the smaller.
        let groups_cost = if tids.len() > SKIP_BLOCK {
            self.groups.len() * GROUPS_BIAS_DEN / GROUPS_BIAS_NUM
        } else {
            self.groups.len() + 1
        };
        let (tag, encoding, body) = if self.sparse.len() < groups_cost {
            (TAG_SPARSE, Encoding::Sparse, &self.sparse)
        } else {
            (TAG_GROUPS, Encoding::Groups, &self.groups)
        };

        let start = out.len();
        assert!((start as u64) <= PAYLOAD_MASK, "postings area too large");
        varint::put(out, tids.len() as u64);
        out.extend_from_slice(body);
        Encoded { value: (tag << TAG_SHIFT) | start as u64, encoding, bytes: out.len() - start }
    }

    fn encode_sparse(&mut self, tids: &[Tid]) {
        self.sparse.clear();
        if tids.len() <= SKIP_BLOCK {
            let mut prev = 0;
            for (i, t) in tids.iter().enumerate() {
                let k = t.key();
                varint::put(&mut self.sparse, if i == 0 { k } else { k - prev });
                prev = k;
            }
            return;
        }
        // Skip table first (in `sparse`), block bodies after (in `blocks`).
        self.blocks.clear();
        let mut prev_first = 0;
        for (b, chunk) in tids.chunks(SKIP_BLOCK).enumerate() {
            let first = chunk[0].key();
            let start = self.blocks.len();
            let mut prev = first;
            for t in &chunk[1..] {
                varint::put(&mut self.blocks, t.key() - prev);
                prev = t.key();
            }
            varint::put(&mut self.sparse, if b == 0 { first } else { first - prev_first });
            varint::put(&mut self.sparse, (self.blocks.len() - start) as u64);
            prev_first = first;
        }
        self.sparse.extend_from_slice(&self.blocks);
    }

    fn encode_groups(&mut self, tids: &[Tid], dir: PageDir<'_>) {
        self.groups.clear();
        let n_groups = tids.chunk_by(|a, b| a.group() == b.group()).count();
        varint::put(&mut self.groups, n_groups as u64);

        let mut prev_group = None;
        for group_tids in tids.chunk_by(|a, b| a.group() == b.group()) {
            let g = group_tids[0].group();
            self.encode_group_body(group_tids, dir);
            varint::put(&mut self.groups, prev_group.map_or(g, |p| g - p) as u64);
            varint::put(&mut self.groups, self.group_body.len() as u64);
            self.groups.extend_from_slice(&self.group_body);
            prev_group = Some(g);
        }
    }

    fn encode_group_body(&mut self, tids: &[Tid], dir: PageDir<'_>) {
        let body = &mut self.group_body;
        body.clear();

        let n_pages = tids.chunk_by(|a, b| a.block == b.block).count();
        body.push((n_pages - 1) as u8);
        if n_pages <= PAGE_LIST_MAX {
            body.extend(tids.chunk_by(|a, b| a.block == b.block).map(|p| p[0].page_in_group()));
        } else {
            let mut pages = PageBits::ZERO;
            for t in tids {
                pages.set(t.page_in_group() as usize);
            }
            for w in pages.0 {
                body.extend_from_slice(&w.to_le_bytes());
            }
        }

        let mut bw = BitWriter::new(body);
        for page_tids in tids.chunk_by(|a, b| a.block == b.block) {
            let width = dir.width(page_tids[0].block);
            let mut bits = OffsetBits::ZERO;
            for t in page_tids {
                assert!((t.offset_bit() as usize) < width, "{t:?} beyond page width {width}");
                bits.set(t.offset_bit() as usize);
            }
            let mut left = width;
            for &w in &bits.0 {
                if left == 0 {
                    break;
                }
                let n = left.min(64);
                bw.write(w, n as u32);
                left -= n;
            }
        }
        bw.finish();
    }
}

/// Appends bits LSB-first.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    /// Bits pending in `acc`; always < 64.
    filled: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        BitWriter { out, acc: 0, filled: 0 }
    }

    fn write(&mut self, v: u64, n: u32) {
        debug_assert!(n <= 64);
        if n == 0 {
            return;
        }
        let v = if n == 64 { v } else { v & ((1u64 << n) - 1) };
        self.acc |= v << self.filled;
        let total = self.filled + n;
        if total >= 64 {
            self.out.extend_from_slice(&self.acc.to_le_bytes());
            self.acc = if self.filled == 0 { 0 } else { v >> (64 - self.filled) };
            self.filled = total - 64;
        } else {
            self.filled = total;
        }
    }

    fn finish(self) {
        let bytes = self.filled.div_ceil(8) as usize;
        self.out.extend_from_slice(&self.acc.to_le_bytes()[..bytes]);
    }
}

/// OR `len` bits of the byte stream `src`, starting at bit `src_bit`, into
/// the word bitmap `dst` at bit `dst_bit`. Needs [`TAIL_PADDING`] readable
/// bytes past the end of the source data.
#[inline]
pub(crate) fn copy_bits_or(src: &[u8], src_bit: usize, dst: &mut [u64], dst_bit: usize, len: usize) {
    let (mut s, mut d, mut left) = (src_bit, dst_bit, len);
    while left > 0 {
        let n = left.min(64);
        let byte = s >> 3;
        let mut v = (u128::from_le_bytes(src[byte..byte + 16].try_into().unwrap()) >> (s & 7)) as u64;
        if n < 64 {
            v &= (1u64 << n) - 1;
        }
        let (w, sh) = (d >> 6, d & 63);
        dst[w] |= v << sh;
        if sh != 0 && sh + n > 64 {
            dst[w + 1] |= v >> (64 - sh);
        }
        s += n;
        d += n;
        left -= n;
    }
}

/// A decoded dictionary value: where one term's postings live.
#[derive(Copy, Clone, Debug)]
pub enum TermPostings<'a> {
    Singleton(Tid),
    Sparse { doc_count: u64, data: &'a [u8], pos: usize },
    Groups { doc_count: u64, data: &'a [u8], pos: usize },
}

impl<'a> TermPostings<'a> {
    /// Interpret a dictionary value against the segment's postings area.
    pub fn from_value(value: u64, postings: &'a [u8]) -> Self {
        let payload = value & PAYLOAD_MASK;
        match value >> TAG_SHIFT {
            TAG_SINGLETON => TermPostings::Singleton(Tid::from_key(payload)),
            tag => {
                let mut pos = payload as usize;
                let doc_count = varint::get(postings, &mut pos);
                if tag == TAG_SPARSE {
                    TermPostings::Sparse { doc_count, data: postings, pos }
                } else {
                    debug_assert_eq!(tag, TAG_GROUPS);
                    TermPostings::Groups { doc_count, data: postings, pos }
                }
            }
        }
    }

    pub fn doc_count(&self) -> u64 {
        match *self {
            TermPostings::Singleton(_) => 1,
            TermPostings::Sparse { doc_count, .. } | TermPostings::Groups { doc_count, .. } => doc_count,
        }
    }

    pub fn encoding(&self) -> Encoding {
        match self {
            TermPostings::Singleton(_) => Encoding::Singleton,
            TermPostings::Sparse { .. } => Encoding::Sparse,
            TermPostings::Groups { .. } => Encoding::Groups,
        }
    }

    pub fn cursor(&self) -> Box<dyn Cursor + 'a> {
        match *self {
            TermPostings::Singleton(tid) => Box::new(SingletonCursor { tid }),
            TermPostings::Sparse { doc_count, data, pos } => {
                Box::new(SparseCursor::new(data, pos, doc_count))
            }
            TermPostings::Groups { doc_count, data, pos } => {
                Box::new(GroupsCursor::new(data, pos, doc_count))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------

struct SingletonCursor {
    tid: Tid,
}

impl Cursor for SingletonCursor {
    fn seek(&mut self, target: u32) -> Option<u32> {
        let g = self.tid.group();
        (g >= target).then_some(g)
    }

    fn pages(&mut self) -> PageBits {
        let mut p = PageBits::ZERO;
        p.set(self.tid.page_in_group() as usize);
        p
    }

    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]) {
        let page = self.tid.page_in_group() as usize;
        if mask.get(page) {
            let bit = space.start(page) + self.tid.offset_bit() as usize;
            out[bit >> 6] |= 1 << (bit & 63);
        }
    }

    fn cost(&self) -> u64 {
        1
    }
}

/// Reads a sparse (gap-coded key list) term one group at a time, jumping
/// through the skip table when a seek goes past whole blocks.
struct SparseCursor<'a> {
    data: &'a [u8],
    pos: usize,
    /// (first key, byte position of the rest, key count) per block. Short
    /// lists are a single block.
    blocks: Vec<(u64, u32, u32)>,
    block: usize,
    /// Keys still to decode in the current block after `next`.
    left_in_block: u32,
    prev_key: u64,
    /// Next undelivered tid (already decoded).
    next: Option<Tid>,
    cur: Option<u32>,
    /// (page, bit) of every tid in the current group, ascending.
    entries: Vec<(u8, u16)>,
    pages: PageBits,
    doc_count: u64,
}

impl<'a> SparseCursor<'a> {
    fn new(data: &'a [u8], mut pos: usize, doc_count: u64) -> Self {
        let n = doc_count as usize;
        let mut blocks = Vec::with_capacity(n.div_ceil(SKIP_BLOCK));
        if n <= SKIP_BLOCK {
            let first = varint::get(data, &mut pos);
            blocks.push((first, pos as u32, n as u32));
        } else {
            let mut first = 0;
            let mut lens = Vec::with_capacity(n.div_ceil(SKIP_BLOCK));
            for b in 0..n.div_ceil(SKIP_BLOCK) {
                first += varint::get(data, &mut pos);
                lens.push(varint::get(data, &mut pos) as u32);
                blocks.push((first, 0, (n - b * SKIP_BLOCK).min(SKIP_BLOCK) as u32));
            }
            let mut at = pos as u32;
            for (blk, len) in blocks.iter_mut().zip(lens) {
                blk.1 = at;
                at += len;
            }
        }
        let mut c = SparseCursor {
            data,
            pos,
            blocks,
            block: 0,
            left_in_block: 0,
            prev_key: 0,
            next: None,
            cur: None,
            entries: Vec::new(),
            pages: PageBits::ZERO,
            doc_count,
        };
        c.enter_block(0);
        c
    }

    fn enter_block(&mut self, b: usize) {
        let (first, start, count) = self.blocks[b];
        self.block = b;
        self.pos = start as usize;
        self.prev_key = first;
        self.left_in_block = count - 1;
        self.next = Some(Tid::from_key(first));
    }

    fn advance(&mut self) {
        if self.left_in_block > 0 {
            self.left_in_block -= 1;
            self.prev_key += varint::get(self.data, &mut self.pos);
            self.next = Some(Tid::from_key(self.prev_key));
        } else if self.block + 1 < self.blocks.len() {
            self.enter_block(self.block + 1);
        } else {
            self.next = None;
        }
    }
}

impl Cursor for SparseCursor<'_> {
    fn seek(&mut self, target: u32) -> Option<u32> {
        if let Some(g) = self.cur {
            if g >= target {
                return Some(g);
            }
        }
        if self.next.is_some_and(|t| t.group() < target) {
            // Jump to the last block that starts before `target`.
            let ahead = &self.blocks[self.block + 1..];
            let k = ahead.partition_point(|b| Tid::from_key(b.0).group() < target);
            if k > 0 {
                self.enter_block(self.block + k);
            }
        }
        while self.next.is_some_and(|t| t.group() < target) {
            self.advance();
        }
        let Some(first) = self.next else {
            self.cur = None;
            return None;
        };
        let g = first.group();
        self.entries.clear();
        self.pages = PageBits::ZERO;
        while let Some(t) = self.next.filter(|t| t.group() == g) {
            self.entries.push((t.page_in_group(), t.offset_bit()));
            self.pages.set(t.page_in_group() as usize);
            self.advance();
        }
        self.cur = Some(g);
        Some(g)
    }

    fn pages(&mut self) -> PageBits {
        self.pages
    }

    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]) {
        for &(page, bit) in &self.entries {
            if mask.get(page as usize) {
                let b = space.start(page as usize) + bit as usize;
                out[b >> 6] |= 1 << (b & 63);
            }
        }
    }

    fn cost(&self) -> u64 {
        self.doc_count
    }
}

/// Reads a two-level-bitmap term. `seek` only touches group headers; a
/// group's page set is parsed on first use; bitstream positions are worked
/// out per *run* of consecutive pages (one run for a term on every page), and
/// only the runs that survive the caller's page mask are copied.
struct GroupsCursor<'a> {
    data: &'a [u8],
    /// Next group header.
    pos: usize,
    groups_left: u64,
    prev_group: Option<u32>,
    cur: Option<u32>,
    body_start: usize,
    parsed: bool,
    pages: PageBits,
    /// Bit position of the current group's bitstream.
    stream_bit: usize,
    doc_count: u64,
}

impl<'a> GroupsCursor<'a> {
    fn new(data: &'a [u8], mut pos: usize, doc_count: u64) -> Self {
        let groups_left = varint::get(data, &mut pos);
        GroupsCursor {
            data,
            pos,
            groups_left,
            prev_group: None,
            cur: None,
            body_start: 0,
            parsed: false,
            pages: PageBits::ZERO,
            stream_bit: 0,
            doc_count,
        }
    }

    fn parse_pages(&mut self) {
        let d = self.data;
        let mut p = self.body_start;
        let n_pages = d[p] as usize + 1;
        p += 1;
        self.pages = PageBits::ZERO;
        if n_pages <= PAGE_LIST_MAX {
            for &page in &d[p..p + n_pages] {
                self.pages.set(page as usize);
            }
            p += n_pages;
        } else {
            for (i, w) in self.pages.0.iter_mut().enumerate() {
                *w = read_u64(d, p + i * 8);
            }
            p += 32;
        }
        self.stream_bit = p * 8;
        self.parsed = true;
    }
}

impl Cursor for GroupsCursor<'_> {
    fn seek(&mut self, target: u32) -> Option<u32> {
        if let Some(g) = self.cur {
            if g >= target {
                return Some(g);
            }
        }
        while self.groups_left > 0 {
            self.groups_left -= 1;
            let delta = varint::get(self.data, &mut self.pos) as u32;
            let g = self.prev_group.map_or(delta, |p| p + delta);
            self.prev_group = Some(g);
            let len = varint::get(self.data, &mut self.pos) as usize;
            let body_start = self.pos;
            self.pos += len;
            if g >= target {
                self.cur = Some(g);
                self.body_start = body_start;
                self.parsed = false;
                return Some(g);
            }
        }
        self.cur = None;
        None
    }

    fn pages(&mut self) -> PageBits {
        if !self.parsed {
            self.parse_pages();
        }
        self.pages
    }

    fn or_into(&mut self, mask: &PageBits, space: &GroupSpace<'_>, out: &mut [u64]) {
        if !self.parsed {
            self.parse_pages();
        }
        let want = *mask & self.pages;
        if want.is_zero() {
            return;
        }
        // Walk this term's page runs (each contiguous in the bitstream) in
        // step with the wanted runs, which always lie inside one of them.
        let mut term_runs = self.pages.runs();
        let (mut run_lo, mut run_hi) = term_runs.next().unwrap();
        let mut run_bit = self.stream_bit;
        let data = self.data;
        for (lo, hi) in want.runs() {
            while run_hi <= lo {
                run_bit += space.start(run_hi) - space.start(run_lo);
                (run_lo, run_hi) = term_runs.next().unwrap();
            }
            let dst = space.start(lo);
            let src = run_bit + dst - space.start(run_lo);
            copy_bits_or(data, src, out, dst, space.start(hi) - dst);
        }
    }

    fn cost(&self) -> u64 {
        self.doc_count
    }
}

#[inline]
fn read_u64(d: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(d[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitstream_roundtrip() {
        // Widths that straddle word and byte boundaries in every way.
        let widths = [1usize, 7, 13, 64, 65, 200, 291, 3, 128, 63];
        let mut pages = Vec::new();
        let mut buf = Vec::new();
        let mut bw = BitWriter::new(&mut buf);
        let mut seed = 0x1234_5678u64;
        for &w in &widths {
            let mut bits = OffsetBits::ZERO;
            for b in 0..w {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if seed >> 63 == 1 {
                    bits.set(b);
                }
            }
            let mut left = w;
            for &word in &bits.0 {
                if left == 0 {
                    break;
                }
                let n = left.min(64);
                bw.write(word, n as u32);
                left -= n;
            }
            pages.push((w, bits));
        }
        bw.finish();
        let total: usize = widths.iter().sum();
        assert_eq!(buf.len(), total.div_ceil(8));
        buf.resize(buf.len() + TAIL_PADDING, 0);
        // Copy each page's bits to a different alignment and compare.
        let mut at = 0;
        for (w, bits) in pages {
            for dst_bit in [0, 1, 37, 63, 64, 100] {
                let mut out = [0u64; 10];
                copy_bits_or(&buf, at, &mut out, dst_bit, w);
                let got: Vec<usize> = (0..w)
                    .filter(|&b| {
                        let x = dst_bit + b;
                        out[x >> 6] >> (x & 63) & 1 == 1
                    })
                    .collect();
                assert_eq!(got, bits.ones(), "width {w} at bit {at} -> {dst_bit}");
                assert_eq!(out.iter().map(|w| w.count_ones()).sum::<u32>(), bits.count());
            }
            at += w;
        }
    }
}
