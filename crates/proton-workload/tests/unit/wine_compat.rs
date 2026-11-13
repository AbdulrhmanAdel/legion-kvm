use super::*;

fn fixture() -> (tempfile::TempDir, Overlay, PathBuf) {
    let root = tempfile::tempdir().expect("test directory");
    for (name, bytes) in [
        ("x86_64-windows/ntdll.dll", b"original".as_slice()),
        ("x86_64-unix/ntdll.so", b"unix".as_slice()),
    ] {
        let path = root.path().join("files/lib/wine").join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        fs::write(path, bytes).expect("module");
    }
    let replacement = root.path().join("replacement.dll");
    fs::write(&replacement, b"repaired").expect("replacement");
    let overlay = Overlay {
        stock_pe_sha256: digest(&root.path().join("files/lib/wine/x86_64-windows/ntdll.dll"))
            .expect("PE digest"),
        stock_unix_sha256: digest(&root.path().join("files/lib/wine/x86_64-unix/ntdll.so"))
            .expect("Unix digest"),
        replacement_sha256: digest(&replacement).expect("replacement digest"),
        migrate_loader: true,
    };
    (root, overlay, replacement)
}

#[test]
fn rejects_each_mismatched_component_before_prefix_changes() {
    for component in [
        "files/lib/wine/x86_64-windows/ntdll.dll",
        "files/lib/wine/x86_64-unix/ntdll.so",
        "replacement.dll",
    ] {
        let (root, overlay, replacement) = fixture();
        fs::write(root.path().join(component), b"different build").expect("mismatch");
        assert!(overlay.validate(root.path(), &replacement).is_err());
    }
}

#[test]
fn prefix_validation_preserves_module_and_external_symlink() {
    let (root, overlay, replacement) = fixture();
    let source = root.path().join("files/lib/wine/x86_64-windows/ntdll.dll");
    let prefix = root.path().join("prefix.dll");
    std::os::unix::fs::symlink(&source, &prefix).expect("prefix link");
    let validated = overlay
        .validate(root.path(), &replacement)
        .expect("matched pair");
    validated.validate_prefix(&prefix).expect("stock prefix");
    assert_eq!(fs::read_link(&prefix).expect("preserved link"), source);
    assert_eq!(fs::read(&source).expect("preserved source"), b"original");
    fs::write(&source, b"custom module").expect("custom module");
    assert!(validated.validate_prefix(&prefix).is_err());
    assert_eq!(
        fs::read(&prefix).expect("preserved custom module"),
        b"custom module"
    );
    assert!(prefix.is_symlink());
}

#[test]
fn replaces_external_prefix_link_without_writing_the_source() {
    let (root, overlay, replacement) = fixture();
    let source = root.path().join("files/lib/wine/x86_64-windows/ntdll.dll");
    let prefix = root.path().join("prefix.dll");
    std::os::unix::fs::symlink(&source, &prefix).expect("prefix link");
    let validated = overlay
        .validate(root.path(), &replacement)
        .expect("matched pair");
    assert!(validated.prepare_prefix(&prefix).expect("publish"));
    assert_eq!(fs::read(&source).expect("stock source"), b"original");
    assert_eq!(fs::read(&prefix).expect("private prefix"), b"repaired");
    assert!(!prefix.is_symlink());
    assert!(!validated.prepare_prefix(&prefix).expect("idempotent reuse"));
}

#[test]
fn preserves_unknown_prefix_and_rejects_replacement_changes() {
    let (root, overlay, replacement) = fixture();
    let validated = overlay
        .validate(root.path(), &replacement)
        .expect("matched pair");
    let prefix = root.path().join("prefix.dll");
    fs::write(&prefix, b"custom module").expect("custom prefix");
    assert!(validated.prepare_prefix(&prefix).is_err());
    assert_eq!(fs::read(&prefix).expect("preserved"), b"custom module");
    fs::write(&prefix, b"original").expect("stock prefix");
    fs::write(&replacement, b"changed").expect("changed overlay");
    assert!(validated.prepare_prefix(&prefix).is_err());
    assert_eq!(fs::read(prefix).expect("preserved stock"), b"original");
}

#[test]
fn staging_collision_preserves_existing_file_and_prefix() {
    let (root, overlay, replacement) = fixture();
    let validated = overlay
        .validate(root.path(), &replacement)
        .expect("matched pair");
    let prefix = root.path().join("prefix.dll");
    fs::write(&prefix, b"original").expect("stock prefix");
    let staging = root
        .path()
        .join(format!(".legion-ntdll-{}.tmp", std::process::id()));
    std::os::unix::fs::symlink(&prefix, &staging).expect("existing staging link");
    assert!(validated.prepare_prefix(&prefix).is_err());
    assert_eq!(fs::read(prefix).expect("preserved prefix"), b"original");
    assert!(staging.is_symlink());
}
