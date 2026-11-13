use super::*;
fn record(kind: u8, name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut data = vec![kind];
    data.extend(u32::try_from(name.len()).expect("name").to_le_bytes());
    data.extend(u64::try_from(bytes.len()).expect("length").to_le_bytes());
    data.extend(0o755_u32.to_le_bytes());
    data.extend(name.as_bytes());
    data.extend(bytes);
    data
}
#[test]
fn archive_preserves_tool_hardlinks_and_guest_absolute_links() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut data = record(1, "bin", &[]);
    data.extend(record(0, "bin/legion", b"owned"));
    data.extend(record(3, "bin/legion-qemu", b"bin/legion"));
    data.extend(record(2, "guest-link", b"/usr/lib"));
    data.push(255);
    unpack(&mut data.as_slice(), root.path())?;
    assert_eq!(
        fs::metadata(root.path().join("bin/legion"))?.ino(),
        fs::metadata(root.path().join("bin/legion-qemu"))?.ino()
    );
    assert_eq!(
        fs::read_link(root.path().join("guest-link"))?,
        Path::new("/usr/lib")
    );
    Ok(())
}
#[test]
fn archive_refuses_paths_outside_staging_and_symlink_traversal() -> Result<()> {
    for name in ["../escape", "/absolute"] {
        let root = tempfile::tempdir()?;
        assert!(unpack(&mut record(0, name, b"x").as_slice(), root.path()).is_err());
    }
    let root = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    let mut data = record(2, "link", external.path().as_os_str().as_encoded_bytes());
    data.extend(record(0, "link/escape", b"x"));
    data.push(255);
    assert!(unpack(&mut data.as_slice(), root.path()).is_err());
    assert!(!external.path().join("escape").exists());
    Ok(())
}
