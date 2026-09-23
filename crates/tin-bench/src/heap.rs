//! Simulated Postgres heap layout, so benchmark tids have realistic density.
//!
//! Models `CREATE TABLE docs (id bigint, body text)` with default fillfactor:
//! 8 KB pages with a 24-byte page header, a 4-byte line pointer per tuple, a
//! 24-byte (MAXALIGNed) tuple header, the 8-byte id, and the text as a varlena
//! (1-byte header under 127 bytes, else 4). Tuples above the TOAST threshold
//! (~2 KB) keep an 18-byte TOAST pointer in the heap instead of the text. We
//! ignore inline compression, which would only pack more tuples per page.

use tin_core::tid::MAX_OFFSET;
use tin_core::Tid;

const BLCKSZ: usize = 8192;
const PAGE_HEADER: usize = 24;
const LINE_POINTER: usize = 4;
const TUPLE_HEADER: usize = 24;
const ID_BYTES: usize = 8;
const TOAST_TUPLE_THRESHOLD: usize = 2032;
const TOAST_POINTER: usize = 18;

pub struct Layout {
    pub tids: Vec<Tid>,
    pub pages: usize,
    pub toasted: usize,
}

fn maxalign(n: usize) -> usize {
    n.div_ceil(8) * 8
}

pub fn layout(docs: &[&str]) -> Layout {
    let mut tids = Vec::with_capacity(docs.len());
    let mut block = 0u32;
    let mut offset = 0u16;
    let mut free = BLCKSZ - PAGE_HEADER;
    let mut toasted = 0;
    for doc in docs {
        let varlena = if doc.len() < 127 { 1 + doc.len() } else { 4 + doc.len() };
        let mut tuple = maxalign(TUPLE_HEADER + ID_BYTES + varlena);
        if tuple > TOAST_TUPLE_THRESHOLD {
            tuple = maxalign(TUPLE_HEADER + ID_BYTES + TOAST_POINTER);
            toasted += 1;
        }
        let need = tuple + LINE_POINTER;
        if need > free || offset == MAX_OFFSET {
            block += 1;
            offset = 0;
            free = BLCKSZ - PAGE_HEADER;
        }
        offset += 1;
        free -= need;
        tids.push(Tid::new(block, offset));
    }
    Layout { tids, pages: block as usize + 1, toasted }
}
