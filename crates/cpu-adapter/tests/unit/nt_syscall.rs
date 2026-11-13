use super::*;
#[repr(C, align(64))]
struct Frame([u8; 640]);
#[test]
fn injected_sse_is_restorable_when_interrupted_sse_was_init() {
    let mut frame = Frame([0; 640]);
    frame.0[464..468].copy_from_slice(&0x4650_5853_u32.to_ne_bytes());
    frame.0[468..472].copy_from_slice(&580_u32.to_ne_bytes());
    frame.0[480..484].copy_from_slice(&576_u32.to_ne_bytes());
    frame.0[512..520].copy_from_slice(&4_u64.to_ne_bytes());
    // SAFETY: owned aligned complete mock frame with valid declared header.
    unsafe {
        mark_sse(frame.0.as_mut_ptr().cast());
    }
    assert_eq!(&frame.0[512..520], &6_u64.to_ne_bytes());
    assert_eq!(&frame.0[520..], &[0; 120]);
}
#[test]
fn legacy_frame_does_not_require_or_modify_an_xsave_extension() {
    let mut frame = Frame([0; 640]);
    frame.0[512..].fill(0xa5);
    // SAFETY: owned aligned complete legacy prefix without XSAVE magic.
    unsafe {
        mark_sse(frame.0.as_mut_ptr().cast());
    }
    assert_eq!(&frame.0[512..], &[0xa5; 128]);
}
