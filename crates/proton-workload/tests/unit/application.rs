use super::*;

#[test]
fn rejects_parent_and_symlink_escape_but_accepts_nested_executable() -> Result<()> {
    let root = tempfile::tempdir()?;
    let application = root.path().join("application");
    std::fs::create_dir_all(application.join("bin"))?;
    std::fs::write(application.join("bin/game.exe"), b"owned fixture")?;
    std::fs::write(root.path().join("outside.exe"), b"outside")?;
    std::os::unix::fs::symlink(
        root.path().join("outside.exe"),
        application.join("link.exe"),
    )?;
    for name in ["../outside.exe", "link.exe", "/outside.exe", "", "bin"] {
        let config = Application {
            executable: name.into(),
            arguments: vec![],
            dll_overrides: String::new(),
            working_directory: None,
        };
        assert!(config.resolve(&application).is_err());
    }
    let config = Application {
        executable: "bin/game.exe".into(),
        arguments: vec!["-windowed".into()],
        dll_overrides: "lsteamclient=d".into(),
        working_directory: None,
    };
    assert_eq!(
        config.resolve(&application)?,
        application.join("bin/game.exe")
    );
    assert_eq!(
        config.resolve_working_directory(&application, &config.resolve(&application)?)?,
        application.join("bin")
    );
    for name in ["..", "../", "/tmp", "link.exe", "bin/game.exe"] {
        let config = Application {
            executable: "bin/game.exe".into(),
            arguments: vec![],
            dll_overrides: String::new(),
            working_directory: Some(name.into()),
        };
        assert!(
            config
                .resolve_working_directory(&application, &application.join("bin/game.exe"))
                .is_err()
        );
    }
    let config = Application {
        working_directory: Some(".".into()),
        ..config
    };
    assert_eq!(
        config.resolve_working_directory(&application, &application.join("bin/game.exe"))?,
        application
    );
    Ok(())
}
