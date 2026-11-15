use super::*;

#[test]
fn cursor_copy_respects_shm_offset_stride_and_transparent_padding() -> Result<()> {
    let mut bytes = vec![0; 40];
    bytes[4..12].copy_from_slice(&[1, 2, 3, 127, 4, 5, 6, 255]);
    bytes[20..28].copy_from_slice(&[7, 8, 9, 255, 10, 11, 12, 255]);
    let data = BufferData {
        offset: 4,
        width: 2,
        height: 2,
        stride: 16,
        format: Format::Argb8888,
    };
    let sprite = copy_sprite(&bytes, data, 1, (1, 1))?;
    assert_eq!(&sprite.pixels[..8], &bytes[4..12]);
    assert_eq!(&sprite.pixels[SIDE * 4..SIDE * 4 + 8], &bytes[20..28]);
    assert!(sprite.pixels[8..SIDE * 4].iter().all(|&b| b == 0));
    assert_eq!(sprite.hotspot, (1, 1));
    assert!(copy_sprite(&bytes[..27], data, 1, (0, 0)).is_err());
    Ok(())
}

#[test]
fn hidpi_cursor_has_logical_size_and_xrgb_is_opaque() -> Result<()> {
    let data = BufferData {
        offset: 0,
        width: 4,
        height: 4,
        stride: 16,
        format: Format::Xrgb8888,
    };
    let sprite = copy_sprite(&[7; 64], data, 2, (1, 1))?;
    assert_eq!(&sprite.pixels[..8], &[7, 7, 7, 255, 7, 7, 7, 255]);
    assert_eq!(sprite.pixels[8], 0);
    assert_eq!(sprite.hotspot, (1, 1));
    assert!(copy_sprite(&[7; 64], data, 0, (0, 0)).is_err());
    assert!(copy_sprite(&[7; 64], BufferData { stride: 1, ..data }, 1, (0, 0)).is_err());
    Ok(())
}
