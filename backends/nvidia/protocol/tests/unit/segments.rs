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
