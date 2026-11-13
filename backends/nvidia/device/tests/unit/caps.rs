use super::*;

#[test]
fn the_default_leaves_compute_out() {
    assert!(!Caps::DEFAULT.has(COMPUTE));
    assert!(Caps::DEFAULT.has(GRAPHICS | VIDEO | UTILITY));
    assert_eq!(Caps::DEFAULT.to_string(), "graphics,video,utility");
}

#[test]
fn a_list_parses_in_any_order_and_case() {
    let c = Caps::parse("Compute, graphics").expect("parses");
    assert_eq!(c.bits(), COMPUTE | GRAPHICS);
    assert_eq!(c.to_string(), "graphics,compute");
}

#[test]
fn a_misspelt_or_empty_list_is_refused() {
    assert!(Caps::parse("graphics,cmopute").is_err());
    assert!(Caps::parse("").is_err());
    assert!(Caps::parse(",").is_err());
}

/// The bits are a contract with the guest driver's NVGPU_CAP_* defines.
#[test]
fn bits_match_the_guest_driver() {
    assert_eq!((COMPUTE, GRAPHICS, VIDEO, UTILITY), (1, 2, 4, 8));
}

#[test]
fn classes_map_to_the_capability_that_serves_them() {
    assert_eq!(Caps::for_class(0xc797), Some(GRAPHICS)); // AMPERE_B
    assert_eq!(Caps::for_class(0xc7b7), Some(VIDEO)); // Ampere NVENC
    assert_eq!(Caps::for_class(0xc9b0), Some(VIDEO)); // Ada NVDEC
    assert_eq!(Caps::for_class(0xc7c0), None); // AMPERE_COMPUTE_B
    assert_eq!(Caps::for_class(0x0080), None); // NV01_DEVICE_0
}

#[test]
fn no_class_is_in_both_lists() {
    for c in THREE_D_CLASSES {
        assert!(!VIDEO_CLASSES.contains(c), "{c:#x}");
    }
}
