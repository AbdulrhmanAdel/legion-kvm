use super::*;

#[test]
fn io_state_requires_a_complete_read_or_write_and_bounded_descriptor() {
    assert_eq!(
        io_call("0 0x7 0x1000 0x40 0x0 0x0 0x0 0x2000 0x3000"),
        Some(IoCall {
            operation: "read",
            fd: 7,
            requested_bytes: 64
        })
    );
    assert_eq!(
        io_call("1 0x8 0x1000 0x18 0x0 0x0 0x0 0x2000 0x3000")
            .expect("write")
            .operation,
        "write"
    );
    for value in [
        "running",
        "-1 0x2000 0x3000",
        "0 0x7",
        "0 0x100000000 0x0 0x40 0x0 0x0 0x0 0x0 0x0",
        "16 0x7 0x1000 0x40 0x0 0x0 0x0 0x2000 0x3000",
    ] {
        assert_eq!(io_call(value), None);
    }
}
