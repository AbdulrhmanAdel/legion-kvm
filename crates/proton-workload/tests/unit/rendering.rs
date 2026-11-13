use super::*;
#[test]
fn shader_oracle_rejects_stale_seeds_and_corruption_with_padded_rows() {
    let mut bytes = [0_u8; 24];
    for y in 0..2 {
        for x in 0..2 {
            let start = y * 12 + x * 4;
            bytes[start..start + 4].copy_from_slice(&shader_pixel(x as u32, y as u32, 17, 8));
        }
    }
    assert_eq!(
        verify_shader(&bytes[..20], 12, 2, 2, 17, 8)
            .expect("correct image")
            .pixels,
        4
    );
    assert!(verify_shader(&bytes, 12, 2, 2, 18, 8).is_none());
    assert!(verify_shader(&bytes[..19], 12, 2, 2, 17, 8).is_none());
    bytes[12] ^= 1;
    assert!(verify_shader(&bytes, 12, 2, 2, 17, 8).is_none());
    assert!(verify_shader(&bytes, 4, 2, 2, 17, 8).is_none());
}
