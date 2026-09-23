//! Pending-list records: one per tuple inserted since the last flush.
//!
//! ```text
//! record := varint(body_len) body
//! body   := u32 block  u16 offset  varint(n_terms)  (varint(len) term_bytes)*
//! ```
//!
//! Terms are the document's distinct analyzed terms, sorted, so queries can
//! evaluate a record with a binary search per term.

use tin_core::{Plan, SortedTerms, Tid};

pub struct Record {
    pub tid: Tid,
    pub terms: Vec<String>,
}

impl Record {
    pub fn matches(&self, plan: &Plan) -> bool {
        plan.matches(&SortedTerms(&self.terms))
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if byte < 0x80 {
            return Some(v);
        }
    }
    None
}

/// Encode one record (`terms` sorted and distinct).
pub fn encode(tid: Tid, terms: &[String]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + terms.iter().map(|t| t.len() + 1).sum::<usize>());
    body.extend_from_slice(&tid.block.to_le_bytes());
    body.extend_from_slice(&tid.offset.to_le_bytes());
    put_varint(&mut body, terms.len() as u64);
    for t in terms {
        put_varint(&mut body, t.len() as u64);
        body.extend_from_slice(t.as_bytes());
    }
    let mut out = Vec::with_capacity(body.len() + 3);
    put_varint(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    out
}

/// Decode a stream of whole records.
pub fn decode_all(b: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < b.len() {
        match decode_one(b, &mut pos) {
            Some(r) => out.push(r),
            None => pgrx::error!("tin: pending list is corrupt at byte {pos}"),
        }
    }
    out
}

fn decode_one(b: &[u8], pos: &mut usize) -> Option<Record> {
    let len = get_varint(b, pos)? as usize;
    let body = b.get(*pos..pos.checked_add(len)?)?;
    *pos += len;
    let block = u32::from_le_bytes(body.get(0..4)?.try_into().ok()?);
    let offset = u16::from_le_bytes(body.get(4..6)?.try_into().ok()?);
    let mut p = 6;
    let n = get_varint(body, &mut p)? as usize;
    let mut terms = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let l = get_varint(body, &mut p)? as usize;
        let t = body.get(p..p.checked_add(l)?)?;
        p += l;
        terms.push(String::from_utf8(t.to_vec()).ok()?);
    }
    (p == body.len() && offset >= 1).then(|| Record { tid: Tid::new(block, offset), terms })
}
