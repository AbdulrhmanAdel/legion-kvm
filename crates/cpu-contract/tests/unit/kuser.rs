use super::*;
#[test]
fn exact_scope_preserves_dispatch_xstate_and_clock_fields() {
    for vendor in [Vendor::Amd, Vendor::Intel] {
        let before = core::array::from_fn(|at| (at as u8).wrapping_mul(71));
        let mut page = before;
        apply(vendor, &mut page);
        assert!(matches(vendor, &page));
        for range in [
            0x8..0x20,
            0x308..0x30c,
            0x320..0x32c,
            0x340..0x358,
            0x3d8..0x818,
            0xffc..0x1000,
        ] {
            assert_eq!(page[range.clone()], before[range]);
        }
        assert_eq!(page[0x295], 1);
        assert_eq!(page[0x297], if vendor == Vendor::Intel { 0 } else { 1 });
        let once = page;
        apply(vendor, &mut page);
        assert_eq!(page, once);
    }
}
