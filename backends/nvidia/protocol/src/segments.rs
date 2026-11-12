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
mod tests {
    use super::*;

    #[test]
    fn a_table_round_trips() {
        let a = [1u8, 2, 3];
        let b = [9u8; 16];
        let mut buf = [0u8; 128];
        let n = encode(&mut buf, &[(8, &a), (24, &b)]).expect("encodes");
        assert_eq!(n, encoded_len(&[3, 16]).unwrap());

        let s = Segments::parse(&buf[..n]).expect("parses");
        assert_eq!(s.len(), 2);
        assert_eq!(s.find(8), Some(&a[..]));
        assert_eq!(s.find(24), Some(&b[..]));
        assert_eq!(s.find(16), None);
    }

    #[test]
    fn an_empty_table_is_valid_and_holds_nothing() {
        let mut buf = [0u8; 8];
        let n = encode(&mut buf, &[]).expect("encodes");
        assert_eq!(n, 8);
        let s = Segments::parse(&buf[..n]).expect("parses");
        assert!(s.is_empty());
        assert_eq!(s.find(0), None);
    }

    #[test]
    fn a_zero_length_segment_keeps_its_place() {
        let mut buf = [0u8; 64];
        let n = encode(&mut buf, &[(8, &[]), (16, &[7u8, 7])]).expect("encodes");
        let s = Segments::parse(&buf[..n]).expect("parses");
        assert_eq!(s.find(8), Some(&[][..]));
        assert_eq!(s.find(16), Some(&[7u8, 7][..]));
    }

    /// Every one of these is a block a guest can send.
    #[test]
    fn a_malformed_block_is_refused_rather_than_read() {
        assert!(Segments::parse(&[]).is_none());
        assert!(Segments::parse(&[0u8; 4]).is_none());

        // A count past the limit.
        let mut buf = [0u8; 128];
        buf[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(Segments::parse(&buf).is_none());

        // A count whose table does not fit.
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(Segments::parse(&buf).is_none());

        // A length that runs past the block.
        let mut buf = [0u8; 24];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&8u32.to_le_bytes());
        buf[12..16].copy_from_slice(&4096u32.to_le_bytes());
        assert!(Segments::parse(&buf).is_none());

        // A length that overflows the running total.
        let mut buf = [0u8; 32];
        buf[0..4].copy_from_slice(&2u32.to_le_bytes());
        buf[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        buf[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Segments::parse(&buf).is_none());
    }

    #[test]
    fn too_many_segments_do_not_encode() {
        let mut buf = [0u8; 512];
        let one: &[u8] = &[0u8; 4];
        let many: [(u32, &[u8]); 9] = [(0, one); 9];
        assert!(encode(&mut buf, &many).is_none());
        assert!(encoded_len(&[4; 9]).is_none());
    }

    #[test]
    fn a_buffer_too_small_does_not_encode() {
        let mut buf = [0u8; 16];
        assert!(encode(&mut buf, &[(8, &[0u8; 32])]).is_none());
    }

    /// The sentinel has to be unreachable as a real offset, or a guest could send one pointer and have it read as a table.
    #[test]
    fn the_sentinel_is_not_a_plausible_offset() {
        assert_eq!(SEGMENTED, u32::MAX);
        assert!(SEGMENTED as usize > 64 * 1024);
    }
}
