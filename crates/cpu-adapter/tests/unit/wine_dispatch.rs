use super::*;
#[test]
fn return_mapping_requires_the_complete_selector_syscall_and_dispatcher_layout() {
    let mut bytes = [
        0x4c, 0x8b, 0xd1, 0xb8, 15, 0, 0, 0, 0xf6, 4, 0x25, 8, 3, 0xfe, 0x7f, 1, 0x75, 3, 0x0f, 5,
        0xc3, 0xeb, 1, 0xc3, 0xff, 0x14, 0x25, 0, 0x10, 0xfe, 0x7f, 0xc3,
    ];
    assert_eq!(decode_return(0x101f, 15, &bytes), Some(0x1014));
    assert_eq!(decode_return(0x101f, 16, &bytes), None);
    assert_eq!(decode_return(1, 15, &bytes), None);
    for at in [0, 8, 16, 18, 20, 24, 27, 31] {
        bytes[at] ^= 1;
        assert_eq!(decode_return(0x101f, 15, &bytes), None);
        bytes[at] ^= 1;
    }
}
