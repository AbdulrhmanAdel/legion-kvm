use crate::Vendor;

pub const PAGE_SIZE: usize = 4096;

pub fn visit_writes(vendor: Vendor, mut write: impl FnMut(usize, u8)) {
    let root = b"C:\\Windows\0";
    for offset in 0..0x104 {
        write(
            0x30 + offset,
            if offset % 2 == 0 {
                root.get(offset / 2).copied().unwrap_or(0)
            } else {
                0
            },
        );
    }
    for &(offset, width, value) in WRITES {
        for (index, byte) in value.to_le_bytes()[..width].iter().copied().enumerate() {
            write(offset + index, byte);
        }
    }
    if vendor == Vendor::Intel {
        write(0x297, 0);
    }
    write(0x295, 1);
}

pub fn apply(vendor: Vendor, page: &mut [u8; PAGE_SIZE]) {
    visit_writes(vendor, |offset, byte| page[offset] = byte);
}

/// Verify the final byte values, including overlapping initialization writes.
pub fn matches(vendor: Vendor, page: &[u8; PAGE_SIZE]) -> bool {
    let mut expected = *page;
    apply(vendor, &mut expected);
    expected == *page
}

const WRITES: &[(usize, usize, u64)] = &[
    (0x260, 8, 0x0100006658),
    (0x268, 4, 0x090001),
    (0x26c, 4, 0xa),
    (0x270, 4, 0),
    (0x274, 4, 0x01010000),
    (0x278, 4, 0x010000),
    (0x27c, 4, 0x010101),
    (0x280, 4, 0x010101),
    (0x284, 4, 0x0100),
    (0x288, 4, 0x01010101),
    (0x28c, 4, 0),
    (0x290, 4, 1),
    (0x294, 4, 0x01000101),
    (0x298, 4, 0x01010101),
    (0x29c, 4, 0x010001),
    (0x2a0, 4, 0),
    (0x2a4, 4, 0),
    (0x2a8, 4, 0),
    (0x2ac, 4, 0),
    (0x2b0, 4, 1),
    (0x2d0, 8, 0x320a0000000110),
    (0x2e8, 8, 0x0100007fb10b),
    (0x2f4, 4, 0),
    (0x36c, 8, 0),
    (0x374, 8, 0),
    (0x37c, 4, 1),
    (0x3c0, 8, 0x83000100000010),
];

#[cfg(test)]
mod tests {
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
}
