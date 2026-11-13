use super::*;
#[test]
fn rejects_mismatched_gateway_continuation_and_null_hook() {
    let prologue = [0x48, 0x55, 0x48, 0x89, 0xe5];
    let mut pair = [0_u8; 33];
    pair[..5].copy_from_slice(&prologue);
    pair[5..11].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    pair[19..25].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    pair[11..19].copy_from_slice(&0x1005_u64.to_le_bytes());
    pair[25..].copy_from_slice(&0x3000_u64.to_le_bytes());
    assert_eq!(validate_pair(0x1000, &prologue, &pair), Some(0x3000));
    for at in [0, 5, 11, 19] {
        let mut bad = pair;
        bad[at] ^= 1;
        assert_eq!(validate_pair(0x1000, &prologue, &bad), None);
    }
    pair[25..].fill(0);
    assert_eq!(validate_pair(0x1000, &prologue, &pair), None);
}
