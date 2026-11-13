use super::*;

#[test]
fn preserves_existing_proton_metadata_from_either_path() -> Result<()> {
    let root = tempfile::tempdir()?;
    let compat = root.path().join("compat");
    let pfx = compat.join("pfx");
    fs::create_dir_all(pfx.join("drive_c"))?;
    fs::write(compat.join("tracked_files"), b"existing ledger")?;
    for selected in [&compat, &pfx] {
        let paths = resolve(selected, &root.path().join("state"))?;
        assert_eq!(paths.prefix, pfx);
        assert_eq!(paths.compatdata, compat);
        assert_eq!(fs::read(compat.join("tracked_files"))?, b"existing ledger");
    }
    Ok(())
}

#[test]
fn recognizes_a_prefix_with_a_self_referencing_proton_link() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("drive_c"))?;
    symlink(".", root.path().join("pfx"))?;
    let paths = resolve(root.path(), &root.path().join("state"))?;
    assert_eq!(paths.prefix, root.path());
    assert_eq!(paths.compatdata, root.path());
    Ok(())
}

#[test]
fn plain_prefixes_keep_their_contents_and_separate_their_ledgers() -> Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let first = resolve(&root.path().join("first"), &state)?;
    fs::write(first.prefix.join("user.reg"), b"saved registry")?;
    fs::write(first.compatdata.join("tracked_files"), b"ledger")?;
    let repeated = resolve(&first.prefix, &state)?;
    let second = resolve(&root.path().join("second"), &state)?;
    assert_eq!(first.compatdata, repeated.compatdata);
    assert_ne!(first.compatdata, second.compatdata);
    assert_eq!(fs::read(first.prefix.join("user.reg"))?, b"saved registry");
    assert_eq!(fs::read(first.compatdata.join("tracked_files"))?, b"ledger");
    Ok(())
}
