//! Shared, immutable byte buffers: a segment's dictionary and postings can
//! live in memory the segment doesn't own (a Postgres shared-memory mapping,
//! shared by every backend) as well as in its own `Vec`.

use std::ops::{Deref, Range};
use std::sync::Arc;

/// Memory a [`Bytes`] can point into.
pub trait Backing: Send + Sync {
    fn bytes(&self) -> &[u8];
}

impl Backing for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

/// A cheaply clonable view of part of a [`Backing`].
#[derive(Clone)]
pub struct Bytes {
    buf: Arc<dyn Backing>,
    range: Range<usize>,
}

impl Bytes {
    pub fn new(buf: Arc<dyn Backing>) -> Bytes {
        let len = buf.bytes().len();
        Bytes { buf, range: 0..len }
    }

    pub fn from_vec(v: Vec<u8>) -> Bytes {
        Bytes::new(Arc::new(v))
    }

    /// A sub-range of this view.
    pub fn slice(&self, r: Range<usize>) -> Bytes {
        assert!(r.start <= r.end && r.end <= self.range.len(), "slice out of range");
        Bytes { buf: self.buf.clone(), range: self.range.start + r.start..self.range.start + r.end }
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf.bytes()[self.range.clone()]
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({} bytes)", self.len())
    }
}
