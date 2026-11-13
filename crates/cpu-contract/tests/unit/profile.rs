use super::*;

#[test]
fn reference_profiles_and_passthrough() {
    let native = Registers::new(0, 0xab12_3456, 0x1234_5678, 0xffff_ffff);
    assert_eq!(
        reply(Vendor::Intel, 1, 0, native),
        Some(Registers::new(
            0xd0670,
            0xab10_0800,
            0x7ffa_fbff,
            0xbfeb_fbff
        ))
    );
    assert_eq!(
        reply(Vendor::Amd, 0x8000_001d, 3, native),
        Some(Registers::new(0x3c163, 0x3c0003f, 0x7fff, 1))
    );
    assert_eq!(
        reply(Vendor::Intel, 7, 1, native),
        Some(Registers::default())
    );
    assert_eq!(
        reply(Vendor::Amd, 7, 0, native),
        Some(Registers::new(0, 0x219c_97a9, 0x0040_068c, 0x10))
    );
    assert_eq!(reply(Vendor::Amd, 7, 1, native), Some(Registers::default()));
    assert_eq!(
        reply(Vendor::Intel, 7, 0, native),
        Some(Registers::new(
            0,
            (native.ebx & 0x23dc_bfff) | 0x180_0000,
            (native.ecx & 0xffc1_a7bd) | 0x40_0200,
            (native.edx & 0xff7f_fef3) | 0x4000
        ))
    );
    assert_eq!(reply(Vendor::Intel, 0xb, 0, native), None);
    assert_eq!(reply(Vendor::Amd, 0x8000_001e, 0, native), None);
    assert_eq!(
        reply(Vendor::Intel, 0x1234, 0, native),
        Some(Registers::default())
    );
}

#[test]
fn intel_signed_results_match_full_reference_registers() {
    let reply = Registers::new(1, 0x8000_0000, 0x8000_0001, 0xffff_ffff);
    assert_eq!(
        guest_registers(Vendor::Intel, 7, 0, false, true, reply),
        [1, 0x8000_0000, 0xffff_ffff_8000_0001, u64::MAX]
    );
    assert_eq!(
        guest_registers(Vendor::Intel, 0xb, 0, true, true, reply),
        [1, 0xffff_ffff_8000_0000, 0xffff_ffff_8000_0001, u64::MAX]
    );
    assert_eq!(
        guest_registers(Vendor::Amd, 7, 0, false, true, reply),
        [1, 0x8000_0000, 0x8000_0001, 0xffff_ffff]
    );
    assert_eq!(
        guest_registers(Vendor::Intel, 7, 0, false, false, reply),
        [1, 0x8000_0000, 0x8000_0001, 0xffff_ffff]
    );
}

#[test]
fn protocol_register_comparisons_follow_each_reference_handler() {
    let upper_bits = 1_u64 << 32;
    assert_eq!(
        control_request(Vendor::Amd, upper_bits | 0x1337),
        ControlRequest::Register
    );
    assert_eq!(
        control_request(Vendor::Intel, upper_bits | 0x1337),
        ControlRequest::Other
    );
    for vendor in [Vendor::Amd, Vendor::Intel] {
        assert_eq!(control_request(vendor, 0x1338), ControlRequest::Ready);
        assert_eq!(control_request(vendor, 0x336933), ControlRequest::Handler);
        assert_eq!(
            control_request(vendor, upper_bits | 0x1338),
            ControlRequest::Other
        );
        assert_eq!(
            control_request(vendor, upper_bits | 0x336933),
            ControlRequest::Other
        );
    }
}
