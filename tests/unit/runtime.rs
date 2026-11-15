use super::*;

fn fixture(root: &Path) -> Runtime {
    let manifest = serde_json::from_value(serde_json::json!({
        "schema":2,"qemu":"owned","qemu_data":"owned",
        "renderer":"owned","virtiofsd":"owned","host_loader":"owned",
        "host_libraries":"owned","kernel":"owned","init":"owned",
        "modules":["owned"],"userspace":"owned","workload_config":"owned"
    }))
    .expect("fixture manifest");
    Runtime {
        root: root.to_owned(),
        manifest,
    }
}

#[test]
fn runtime_assets_reject_parent_absolute_and_symlink_escapes() {
    let directory = tempfile::tempdir().expect("private fixture");
    let root = directory.path().join("bundle");
    fs::create_dir(&root).expect("bundle root");
    fs::write(root.join("owned"), b"fixture").expect("owned asset");
    fs::write(directory.path().join("outside"), b"outside").expect("outside asset");
    std::os::unix::fs::symlink(directory.path().join("outside"), root.join("escape"))
        .expect("escape fixture");
    let runtime = fixture(&root);
    assert_eq!(
        runtime.asset(Path::new("owned")).expect("owned"),
        root.join("owned")
    );
    for value in ["../outside", "/etc/passwd", "escape", ""] {
        assert!(runtime.asset(Path::new(value)).is_err(), "{value}");
    }
}
