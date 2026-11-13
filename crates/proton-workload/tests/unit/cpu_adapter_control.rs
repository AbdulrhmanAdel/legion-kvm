use super::*;
#[test]
fn never_reuses_previous_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("state");
    create_state(&path)?;
    fs::write(&path, b"previous run")?;
    assert!(create_state(&path).is_err());
    assert_eq!(fs::read(&path)?, b"previous run");
    Ok(())
}
#[test]
fn requires_initialized_and_exercised_adapter() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("state");
    create_state(&path)?;
    assert!(read_state(&path).is_err());
    let mut bytes = [0_u8; 176];
    bytes[..8].copy_from_slice(b"LCPID001");
    fs::write(&path, bytes)?;
    assert!(read_state(&path).is_err());
    bytes[40..48].copy_from_slice(&1_u64.to_le_bytes());
    fs::write(&path, bytes)?;
    assert_eq!(read_state(&path)?["queries"], 1);
    Ok(())
}
