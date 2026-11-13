pub const SEGMENTED: u32 = u32::MAX;

/// The most segments one message may carry.
pub const MAX_SEGMENTS: usize = 8;

/// Bytes of table for `n` segments.
pub const fn table_bytes(n: usize) -> usize {
    8 + n * 8
}

const fn pad8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// How many bytes a segmented deep block takes, for segments of these lengths.
pub fn encoded_len(lens: &[usize]) -> Option<usize> {
    if lens.len() > MAX_SEGMENTS {
        return None;
    }
    let mut total = table_bytes(lens.len());
    for &l in lens {
        total = total.checked_add(pad8(l))?;
    }
    (total <= u32::MAX as usize).then_some(total)
}

/// Write a segment table and its payloads into `out`.
pub fn encode(out: &mut [u8], segments: &[(u32, &[u8])]) -> Option<usize> {
    let n = segments.len();
    if n > MAX_SEGMENTS {
        return None;
    }
    let mut lens = [0usize; MAX_SEGMENTS];
    for (i, (_, b)) in segments.iter().enumerate() {
        lens[i] = b.len();
    }
    let total = encoded_len(&lens[..n])?;
    let out = out.get_mut(..total)?;
    out.fill(0);

    out[0..4].copy_from_slice(&(n as u32).to_le_bytes());
    let mut at = table_bytes(n);
    for (i, (off, bytes)) in segments.iter().enumerate() {
        let e = 8 + i * 8;
        out[e..e + 4].copy_from_slice(&off.to_le_bytes());
        out[e + 4..e + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += pad8(bytes.len());
    }
    Some(total)
}

/// A segment table and its payloads, as they arrived.
#[derive(Clone, Copy, Debug)]
pub struct Segments<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> Segments<'a> {
    /// Read a segmented deep block.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
        if count > MAX_SEGMENTS {
            return None;
        }
        let mut at = table_bytes(count);
        if bytes.len() < at {
            return None;
        }
        for i in 0..count {
            let e = 8 + i * 8;
            let len = u32::from_le_bytes(bytes[e + 4..e + 8].try_into().ok()?) as usize;
            at = at.checked_add(pad8(len))?;
            if at > bytes.len() {
                return None;
            }
        }
        Some(Segments { bytes, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// `(ptr_offset, payload)` for each segment, in the order they were sent.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &'a [u8])> + '_ {
        let bytes = self.bytes;
        let mut at = table_bytes(self.count);
        (0..self.count).map(move |i| {
            let e = 8 + i * 8;
            let off = u32::from_le_bytes(bytes[e..e + 4].try_into().expect("checked in parse"));
            let len = u32::from_le_bytes(bytes[e + 4..e + 8].try_into().expect("checked in parse"))
                as usize;
            let payload = &bytes[at..at + len];
            at += pad8(len);
            (off, payload)
        })
    }

    /// The payload for one pointer offset, if the guest sent one.
    pub fn find(&self, ptr_offset: u32) -> Option<&'a [u8]> {
        self.iter().find(|(o, _)| *o == ptr_offset).map(|(_, b)| b)
    }
}

#[cfg(test)]
#[path = "../tests/unit/segments.rs"]
mod tests;
