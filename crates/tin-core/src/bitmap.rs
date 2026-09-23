//! The two bitmap levels.
//!
//! * [`PageBits`]: 256 bits, one per heap page in a group. Fits one AVX2 register.
//! * [`OffsetBits`]: 512 bits, one per line pointer on a page (only the first
//!   [`MAX_OFFSET`](crate::tid::MAX_OFFSET) are used). Fits one AVX-512 register
//!   or two AVX2 registers.
//!
//! The operators are plain word-wise loops over aligned fixed-size arrays; with
//! `-C target-cpu=native` rustc lowers them to single vector instructions, and
//! `count_ones` to `POPCNT` / `VPOPCNTQ`.

use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

macro_rules! bitset {
    ($name:ident, $words:expr, $align:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Copy, Clone, PartialEq, Eq, Default)]
        #[repr(C, align($align))]
        pub struct $name(pub [u64; $words]);

        impl $name {
            pub const WORDS: usize = $words;
            pub const BITS: usize = $words * 64;
            pub const ZERO: Self = $name([0; $words]);

            #[inline]
            pub fn is_zero(&self) -> bool {
                self.0.iter().fold(0, |acc, w| acc | w) == 0
            }

            #[inline]
            pub fn count(&self) -> u32 {
                self.0.iter().map(|w| w.count_ones()).sum()
            }

            #[inline]
            pub fn set(&mut self, bit: usize) {
                self.0[bit >> 6] |= 1u64 << (bit & 63);
            }

            #[inline]
            pub fn get(&self, bit: usize) -> bool {
                self.0[bit >> 6] & (1u64 << (bit & 63)) != 0
            }

            /// `self & !other`.
            #[inline]
            pub fn and_not(self, other: Self) -> Self {
                let mut out = self;
                for i in 0..$words {
                    out.0[i] &= !other.0[i];
                }
                out
            }

            /// Calls `f` with each set bit index, ascending.
            #[inline]
            pub fn for_each(&self, mut f: impl FnMut(usize)) {
                for (i, &word) in self.0.iter().enumerate() {
                    let mut w = word;
                    while w != 0 {
                        f(i * 64 + w.trailing_zeros() as usize);
                        w &= w - 1;
                    }
                }
            }

            /// Lowest set bit.
            #[inline]
            pub fn first(&self) -> Option<usize> {
                self.0
                    .iter()
                    .enumerate()
                    .find(|(_, &w)| w != 0)
                    .map(|(i, w)| i * 64 + w.trailing_zeros() as usize)
            }

            /// Highest set bit.
            #[inline]
            pub fn last(&self) -> Option<usize> {
                self.0
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, &w)| w != 0)
                    .map(|(i, w)| i * 64 + 63 - w.leading_zeros() as usize)
            }

            /// Maximal runs of set bits as `(lo, hi)` ranges, ascending.
            #[inline]
            pub fn runs(&self) -> Runs<$words> {
                Runs::new(self.0)
            }

            /// Set bit indexes, ascending.
            pub fn ones(&self) -> Vec<usize> {
                let mut v = Vec::with_capacity(self.count() as usize);
                self.for_each(|b| v.push(b));
                v
            }
        }

        impl BitAnd for $name {
            type Output = Self;
            #[inline]
            fn bitand(mut self, rhs: Self) -> Self {
                self &= rhs;
                self
            }
        }

        impl BitAndAssign for $name {
            #[inline]
            fn bitand_assign(&mut self, rhs: Self) {
                for i in 0..$words {
                    self.0[i] &= rhs.0[i];
                }
            }
        }

        impl BitOr for $name {
            type Output = Self;
            #[inline]
            fn bitor(mut self, rhs: Self) -> Self {
                self |= rhs;
                self
            }
        }

        impl BitOrAssign for $name {
            #[inline]
            fn bitor_assign(&mut self, rhs: Self) {
                for i in 0..$words {
                    self.0[i] |= rhs.0[i];
                }
            }
        }

        impl Not for $name {
            type Output = Self;
            #[inline]
            fn not(mut self) -> Self {
                for i in 0..$words {
                    self.0[i] = !self.0[i];
                }
                self
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}{:?}", stringify!($name), self.ones())
            }
        }
    };
}

/// Iterator over the runs of set bits in a bitset; see `PageBits::runs`.
///
/// Run boundaries are computed word-parallel up front (`starts`: first bit of
/// each run, `ends`: last bit of each run), so each run costs two
/// trailing-zero counts regardless of its length.
pub struct Runs<const W: usize> {
    starts: [u64; W],
    ends: [u64; W],
    ws: usize,
    we: usize,
}

impl<const W: usize> Runs<W> {
    #[inline]
    fn new(words: [u64; W]) -> Self {
        let mut starts = [0u64; W];
        let mut ends = [0u64; W];
        for i in 0..W {
            let prev_top = if i > 0 { words[i - 1] >> 63 } else { 0 };
            let next_bottom = if i + 1 < W { words[i + 1] << 63 } else { 0 };
            starts[i] = words[i] & !((words[i] << 1) | prev_top);
            ends[i] = words[i] & !((words[i] >> 1) | next_bottom);
        }
        Runs { starts, ends, ws: 0, we: 0 }
    }
}

impl<const W: usize> Iterator for Runs<W> {
    type Item = (usize, usize);

    #[inline]
    fn next(&mut self) -> Option<(usize, usize)> {
        while self.ws < W && self.starts[self.ws] == 0 {
            self.ws += 1;
        }
        if self.ws == W {
            return None;
        }
        let lo = self.ws * 64 + self.starts[self.ws].trailing_zeros() as usize;
        self.starts[self.ws] &= self.starts[self.ws] - 1;
        // Every start has a matching end at or after it.
        while self.ends[self.we] == 0 {
            self.we += 1;
        }
        let hi = self.we * 64 + self.ends[self.we].trailing_zeros() as usize + 1;
        self.ends[self.we] &= self.ends[self.we] - 1;
        Some((lo, hi))
    }
}

bitset!(PageBits, 4, 32, "Which of a group's 256 heap pages contain a match.");
bitset!(OffsetBits, 8, 64, "Which line pointers on one heap page match (bit = offset - 1).");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops() {
        let mut a = OffsetBits::ZERO;
        let mut b = OffsetBits::ZERO;
        for i in [0, 63, 64, 290] {
            a.set(i);
        }
        for i in [63, 290, 300] {
            b.set(i);
        }
        assert_eq!((a & b).ones(), vec![63, 290]);
        assert_eq!((a | b).ones(), vec![0, 63, 64, 290, 300]);
        assert_eq!(a.and_not(b).ones(), vec![0, 64]);
        assert_eq!((a & b).count(), 2);
        assert!((a & !a).is_zero());
        assert_eq!(std::mem::align_of::<OffsetBits>(), 64);

        let mut p = PageBits::ZERO;
        for i in (3..70).chain([100, 191, 192, 250, 251, 252, 253, 254, 255]) {
            p.set(i);
        }
        let runs: Vec<_> = p.runs().collect();
        assert_eq!(runs, vec![(3, 70), (100, 101), (191, 193), (250, 256)]);
        let mut q = PageBits::ZERO;
        for i in [0, 63, 64, 127, 128, 200] {
            q.set(i);
        }
        assert_eq!(q.runs().collect::<Vec<_>>(), vec![(0, 1), (63, 65), (127, 129), (200, 201)]);
        assert_eq!((p.first(), p.last()), (Some(3), Some(255)));
        assert_eq!((PageBits::ZERO.first(), PageBits::ZERO.last()), (None, None));
        assert_eq!((!PageBits::ZERO).runs().collect::<Vec<_>>(), vec![(0, 256)]);

        // Randomized: runs must tile exactly the set bits, maximally.
        let mut seed = 42u64;
        for _ in 0..2000 {
            let mut b = PageBits::ZERO;
            let density = (seed >> 60) as u32; // 0..16
            for i in 0..256 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if ((seed >> 33) as u32 % 16) < density {
                    b.set(i);
                }
            }
            let runs: Vec<_> = b.runs().collect();
            let mut rebuilt = PageBits::ZERO;
            for w in runs.windows(2) {
                assert!(w[0].1 < w[1].0, "runs must be separated: {runs:?}");
            }
            for &(lo, hi) in &runs {
                assert!(lo < hi);
                for i in lo..hi {
                    rebuilt.set(i);
                }
            }
            assert_eq!(rebuilt, b);
        }
        assert_eq!(PageBits::ZERO.runs().next(), None);
        assert_eq!(std::mem::align_of::<PageBits>(), 32);
    }
}
