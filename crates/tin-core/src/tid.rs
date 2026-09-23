//! Tuple identifiers.
//!
//! TIN does not assign its own document numbers. A posting *is* the Postgres
//! `ctid`: `(block number, line-pointer offset)`. Everything in the index is
//! addressed by that physical location, which is what lets segments merge
//! without renumbering and lets results come back in heap order.

use std::fmt;

/// Highest line-pointer offset on a heap page (`MaxHeapTuplesPerPage` for the
/// default `BLCKSZ = 8192`). Offsets are 1-based, so valid offsets are
/// `1..=MAX_OFFSET`.
pub const MAX_OFFSET: u16 = 291;

/// Heap pages covered by one page-level bitmap (one "group"). 256 bits = one
/// AVX2 register.
pub const PAGES_PER_GROUP: u32 = 256;

/// A Postgres heap tuple identifier (`ctid`).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid {
    pub block: u32,
    pub offset: u16,
}

impl Tid {
    /// Panics if `offset` is outside `1..=MAX_OFFSET`.
    #[inline]
    pub fn new(block: u32, offset: u16) -> Self {
        assert!((1..=MAX_OFFSET).contains(&offset), "ctid offset {offset} outside 1..={MAX_OFFSET}");
        Tid { block, offset }
    }

    /// Group (run of 256 heap pages) this tuple lives in.
    #[inline]
    pub fn group(self) -> u32 {
        self.block >> 8
    }

    /// Page index within its group, `0..256`.
    #[inline]
    pub fn page_in_group(self) -> u8 {
        self.block as u8
    }

    /// Bit index within the page's offset bitmap, `0..MAX_OFFSET`.
    #[inline]
    pub fn offset_bit(self) -> u16 {
        self.offset - 1
    }

    /// Rebuild a tid from its group/page/bit coordinates.
    #[inline]
    pub fn from_parts(group: u32, page: u8, bit: u16) -> Self {
        Tid { block: (group << 8) | page as u32, offset: bit + 1 }
    }

    /// Dense, order-preserving key: `block * 512 + offset_bit`. Used by the
    /// sparse (rare-term) encoding, where gaps between keys are varint-coded.
    #[inline]
    pub fn key(self) -> u64 {
        ((self.block as u64) << 9) | self.offset_bit() as u64
    }

    #[inline]
    pub fn from_key(key: u64) -> Self {
        Tid { block: (key >> 9) as u32, offset: (key & 0x1FF) as u16 + 1 }
    }
}

impl fmt::Debug for Tid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.block, self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip_and_order() {
        let a = Tid::new(0, 1);
        let b = Tid::new(0, MAX_OFFSET);
        let c = Tid::new(1, 1);
        let d = Tid::new(u32::MAX, MAX_OFFSET);
        for t in [a, b, c, d] {
            assert_eq!(Tid::from_key(t.key()), t);
            assert_eq!(Tid::from_parts(t.group(), t.page_in_group(), t.offset_bit()), t);
        }
        assert!(a.key() < b.key() && b.key() < c.key() && c.key() < d.key());
    }

    #[test]
    #[should_panic]
    fn offset_zero_rejected() {
        Tid::new(0, 0);
    }
}
