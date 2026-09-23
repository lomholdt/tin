//! LEB128 unsigned varints.

#[inline]
pub fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decode at `*pos`, advancing it. Panics on truncated input.
#[inline]
pub fn get(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7F) as u64) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let vals = [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX];
        let mut buf = Vec::new();
        for &v in &vals {
            super::put(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(super::get(&buf, &mut pos), v);
        }
        assert_eq!(pos, buf.len());
    }
}
