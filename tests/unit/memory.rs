use super::*;

#[test]
fn alignment_hint_requires_a_bounded_power_of_two() {
    assert_eq!(alignment(Some("2097152\n")), 2097152);
    for hint in [
        None,
        Some("0"),
        Some("4097"),
        Some("1099511627776"),
        Some("invalid"),
    ] {
        assert_eq!(alignment(hint), 4096);
    }
}

#[test]
#[cfg(not(miri))]
fn aligned_ram_keeps_a_shared_size_sealed_descriptor_without_preallocation() {
    let ram = Ram::new(Mode::Aligned, 4).expect("owned RAM");
    let fd = ram._backing.as_ref().expect("retained descriptor");
    let path = format!("/proc/{}/fd/{}", std::process::id(), fd.as_raw_fd());
    let peer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("same-user QEMU-style open");
    assert_eq!(
        peer.metadata().expect("RAM metadata").len(),
        4 * 1024 * 1024
    );
    assert!(ftruncate(fd, 2 * 1024 * 1024).is_err());
    assert!(ram.argument.contains("memory-backend-file"));
    assert!(ram.argument.contains("share=on,align="));
}
