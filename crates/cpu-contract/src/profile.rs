use crate::{Registers, Vendor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlRequest {
    Register,
    Handler,
    Ready,
    Other,
}

pub fn control_request(vendor: Vendor, raw_leaf: u64) -> ControlRequest {
    let registration = if vendor == Vendor::Amd {
        raw_leaf as u32 == crate::REGISTER_PROCESS
    } else {
        raw_leaf == u64::from(crate::REGISTER_PROCESS)
    };
    if registration {
        ControlRequest::Register
    } else if raw_leaf == u64::from(crate::REGISTER_HANDLER) {
        ControlRequest::Handler
    } else if raw_leaf == u64::from(crate::POLL_READY) {
        ControlRequest::Ready
    } else {
        ControlRequest::Other
    }
}

/// Convert the reference handler's 32-bit results to the full guest registers.
pub fn guest_registers(
    vendor: Vendor,
    leaf: u32,
    subleaf: u32,
    native_passthrough: bool,
    reference_width: bool,
    reply: Registers,
) -> [u64; 4] {
    let values = [reply.eax, reply.ebx, reply.ecx, reply.edx];
    core::array::from_fn(|index| {
        let signed = reference_width
            && vendor == Vendor::Intel
            && (native_passthrough || (leaf == 7 && subleaf == 0 && index >= 2));
        if signed {
            (values[index] as i32 as i64) as u64
        } else {
            u64::from(values[index])
        }
    })
}

pub fn reply(vendor: Vendor, leaf: u32, subleaf: u32, native: Registers) -> Option<Registers> {
    if leaf == 0xb || (vendor == Vendor::Amd && leaf == 0x8000_001e) {
        return None;
    }
    if leaf == 1 {
        return Some(match vendor {
            Vendor::Intel => Registers::new(
                0xd0670,
                0x00100800 | (native.ebx & 0xff000000),
                0x7ffafbff,
                0xbfebfbff,
            ),
            Vendor::Amd => Registers::new(
                0xa20f12,
                0x00100800 | (native.ebx & 0xff000000),
                0x7ef8320b,
                0x178bfbff,
            ),
        });
    }
    if leaf == 7 {
        return Some(if subleaf != 0 {
            Registers::default()
        } else {
            match vendor {
                Vendor::Intel => Registers::new(
                    0,
                    (native.ebx & 0x23dcbfff) | 0x1800000,
                    (native.ecx & 0xffc1a7bd) | 0x400200,
                    (native.edx & 0xff7ffef3) | 0x4000,
                ),
                Vendor::Amd => Registers::new(0, 0x219c97a9, 0x40068c, 0x10),
            }
        });
    }
    let row = match (vendor, leaf, subleaf) {
        (Vendor::Intel, 0x0, _) => Registers::new(0x20, 0x756e6547, 0x6c65746e, 0x49656e69),
        (Vendor::Intel, 0x2, _) => Registers::new(0xfeff01, 0xf0, 0x0, 0x0),
        (Vendor::Intel, 0x4, 0x0) => Registers::new(0x3c004121, 0x2c0003f, 0x3f, 0x0),
        (Vendor::Intel, 0x4, 0x1) => Registers::new(0x3c004122, 0x1c0003f, 0x3f, 0x0),
        (Vendor::Intel, 0x4, 0x2) => Registers::new(0x3c004143, 0x3c0003f, 0x7ff, 0x0),
        (Vendor::Intel, 0x4, 0x3) => Registers::new(0x1c03c163, 0x2c0003f, 0x7fff, 0x4),
        (Vendor::Intel, 0x5, _) => Registers::new(0x40, 0x40, 0x3, 0x10101020),
        (Vendor::Intel, 0x6, _) => Registers::new(0xdfcff7, 0x2, 0x409, 0x3),
        (Vendor::Intel, 0xa, _) => Registers::new(0x8300805, 0x0, 0xf, 0x8604),
        (Vendor::Intel, 0xd, 0x0) => Registers::new(0x207, 0x340, 0xa88, 0x0),
        (Vendor::Intel, 0xd, 0x1) => Registers::new(0xf, 0x3d0, 0x19900, 0x0),
        (Vendor::Intel, 0xd, 0x2) => Registers::new(0x100, 0x240, 0x0, 0x0),
        (Vendor::Intel, 0xd, 0x8) => Registers::new(0x80, 0x0, 0x1, 0x0),
        (Vendor::Intel, 0xd, 0x9) => Registers::new(0x8, 0xa80, 0x0, 0x0),
        (Vendor::Intel, 0xd, 0xb) => Registers::new(0x10, 0x0, 0x1, 0x0),
        (Vendor::Intel, 0xd, 0xc) => Registers::new(0x18, 0x0, 0x1, 0x0),
        (Vendor::Intel, 0xd, 0xf) => Registers::new(0x328, 0x0, 0x1, 0x0),
        (Vendor::Intel, 0xd, 0x10) => Registers::new(0x8, 0x0, 0x1, 0x0),
        (Vendor::Intel, 0x14, 0x0) => Registers::new(0x1, 0x5f, 0x7, 0x0),
        (Vendor::Intel, 0x14, 0x1) => Registers::new(0x2490002, 0x3f003f, 0x0, 0x0),
        (Vendor::Intel, 0x15, _) => Registers::new(0x2, 0xb2, 0x249f000, 0x0),
        (Vendor::Intel, 0x16, _) => Registers::new(0xfa0, 0x157c, 0x64, 0x0),
        (Vendor::Intel, 0x18, 0x0) => Registers::new(0x8, 0x0, 0x0, 0x0),
        (Vendor::Intel, 0x18, 0x1) => Registers::new(0x0, 0x80001, 0x20, 0x4022),
        (Vendor::Intel, 0x18, 0x2) => Registers::new(0x0, 0x80006, 0x4, 0x4022),
        (Vendor::Intel, 0x18, 0x3) => Registers::new(0x0, 0x10000f, 0x1, 0x4125),
        (Vendor::Intel, 0x18, 0x4) => Registers::new(0x0, 0x40001, 0x10, 0x4024),
        (Vendor::Intel, 0x18, 0x5) => Registers::new(0x0, 0x40006, 0x8, 0x4024),
        (Vendor::Intel, 0x18, 0x6) => Registers::new(0x0, 0x80008, 0x1, 0x4124),
        (Vendor::Intel, 0x18, 0x7) => Registers::new(0x0, 0x80007, 0x80, 0x4043),
        (Vendor::Intel, 0x18, 0x8) => Registers::new(0x0, 0x80009, 0x80, 0x4043),
        (Vendor::Intel, 0x19, _) => Registers::new(0x7, 0x14, 0x3, 0x0),
        (Vendor::Intel, 0x1b, 0x0) => Registers::new(0x1, 0x1, 0x0, 0x0),
        (Vendor::Intel, 0x1c, _) => Registers::new(0x4000000b, 0x7, 0x7, 0x0),
        (Vendor::Intel, 0x1f, 0x0) => Registers::new(0x1, 0x2, 0x100, 0x0),
        (Vendor::Intel, 0x1f, 0x1) => Registers::new(0x4, 0x10, 0x201, 0x0),
        (Vendor::Intel, 0x20, 0x0) => Registers::new(0x0, 0x1, 0x0, 0x0),
        (Vendor::Intel, 0x80000000, _) => Registers::new(0x80000008, 0x0, 0x0, 0x0),
        (Vendor::Intel, 0x80000001, _) => Registers::new(0x0, 0x0, 0x121, 0x2c100800),
        (Vendor::Intel, 0x80000002, _) => {
            Registers::new(0x65746e49, 0x2952286c, 0x726f4320, 0x4d542865)
        }
        (Vendor::Intel, 0x80000003, _) => Registers::new(0x20352029, 0x50333232, 0x4551, 0x0),
        (Vendor::Intel, 0x80000006, _) => Registers::new(0x0, 0x0, 0x8007040, 0x0),
        (Vendor::Intel, 0x80000007, _) => Registers::new(0x0, 0x0, 0x0, 0x100),
        (Vendor::Intel, 0x80000008, _) => Registers::new(0x302e, 0x0, 0x0, 0x0),
        (Vendor::Amd, 0x0, _) => Registers::new(0x10, 0x68747541, 0x444d4163, 0x69746e65),
        (Vendor::Amd, 0x5, _) => Registers::new(0x40, 0x40, 0x3, 0x11),
        (Vendor::Amd, 0x6, _) => Registers::new(0x4, 0x0, 0x1, 0x0),
        (Vendor::Amd, 0xd, 0x0) => Registers::new(0x207, 0x340, 0x988, 0x0),
        (Vendor::Amd, 0xd, 0x1) => Registers::new(0xf, 0x350, 0x1800, 0x0),
        (Vendor::Amd, 0xd, 0x2) => Registers::new(0x100, 0x240, 0x0, 0x0),
        (Vendor::Amd, 0xd, 0x9) => Registers::new(0x8, 0x980, 0x0, 0x0),
        (Vendor::Amd, 0xd, 0xb) => Registers::new(0x10, 0x0, 0x1, 0x0),
        (Vendor::Amd, 0xd, 0xc) => Registers::new(0x18, 0x0, 0x1, 0x0),
        (Vendor::Amd, 0xf, 0x0) => Registers::new(0x0, 0xff, 0x0, 0x2),
        (Vendor::Amd, 0xf, 0x1) => Registers::new(0x0, 0x40, 0xff, 0x7),
        (Vendor::Amd, 0x10, 0x0) => Registers::new(0x0, 0x2, 0x0, 0x0),
        (Vendor::Amd, 0x10, 0x1) => Registers::new(0xf, 0x0, 0x4, 0xf),
        (Vendor::Amd, 0x80000000, _) => {
            Registers::new(0x80000023, 0x68747541, 0x444d4163, 0x69746e65)
        }
        (Vendor::Amd, 0x80000001, _) => {
            Registers::new(0xa20f12, 0x20000000, 0x75c237ff, 0x2fd3fbff)
        }
        (Vendor::Amd, 0x80000002, _) => {
            Registers::new(0x32304744, 0x42545253, 0x464d3450, 0x20202041)
        }
        (Vendor::Amd, 0x80000003, _) => {
            Registers::new(0x20202020, 0x20202020, 0x20202020, 0x20202020)
        }
        (Vendor::Amd, 0x80000004, _) => {
            Registers::new(0x20202020, 0x20202020, 0x20202020, 0x202020)
        }
        (Vendor::Amd, 0x80000005, _) => {
            Registers::new(0xff40ff40, 0xff40ff40, 0x20080140, 0x20080140)
        }
        (Vendor::Amd, 0x80000006, _) => {
            Registers::new(0x48002200, 0x68004200, 0x2006140, 0x1009140)
        }
        (Vendor::Amd, 0x80000007, _) => Registers::new(0x0, 0x3b, 0x0, 0x6799),
        (Vendor::Amd, 0x80000008, _) => Registers::new(0x3030, 0x111ef657, 0x400f, 0x10000),
        (Vendor::Amd, 0x8000000a, _) => Registers::new(0x1, 0x8000, 0x0, 0x101bbcff),
        (Vendor::Amd, 0x80000019, _) => Registers::new(0xf040f040, 0xf0400000, 0x0, 0x0),
        (Vendor::Amd, 0x8000001a, _) => Registers::new(0x6, 0x0, 0x0, 0x0),
        (Vendor::Amd, 0x8000001b, _) => Registers::new(0x3ff, 0x0, 0x0, 0x0),
        (Vendor::Amd, 0x8000001d, 0x0) => Registers::new(0x4121, 0x1c0003f, 0x3f, 0x0),
        (Vendor::Amd, 0x8000001d, 0x1) => Registers::new(0x4122, 0x1c0003f, 0x3f, 0x0),
        (Vendor::Amd, 0x8000001d, 0x2) => Registers::new(0x4143, 0x1c0003f, 0x3ff, 0x2),
        (Vendor::Amd, 0x8000001d, 0x3) => Registers::new(0x3c163, 0x3c0003f, 0x7fff, 0x1),
        (Vendor::Amd, 0x8000001f, _) => Registers::new(0x1780f, 0x173, 0x1fd, 0x1),
        (Vendor::Amd, 0x80000020, 0x0) => Registers::new(0x0, 0x2, 0x0, 0x0),
        (Vendor::Amd, 0x80000020, 0x1) => Registers::new(0xb, 0x0, 0x0, 0xf),
        (Vendor::Amd, 0x80000021, _) => Registers::new(0x4d, 0x0, 0x0, 0x0),
        _ => Registers::default(),
    };
    Some(row)
}

#[cfg(test)]
mod tests {
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
}
