use super::*;
#[test]
fn steam_command_keeps_selected_proton_and_game_argument_boundaries() -> Result<()> {
    let root = tempfile::tempdir()?;
    let proton = root.path().join("Proton with spaces");
    fs::create_dir_all(proton.join("files/bin"))?;
    fs::write(proton.join("files/bin/wine"), b"owned")?;
    fs::write(proton.join("proton"), b"owned")?;
    let game = root.path().join("A game");
    fs::create_dir_all(game.join("Engine"))?;
    fs::create_dir_all(game.join("bin"))?;
    let exe = game.join("bin/Game.exe");
    fs::write(&exe, b"owned")?;
    let args = vec![
        OsString::from("container-wrapper"),
        OsString::from("--"),
        proton.join("proton").into_os_string(),
        "waitforexitandrun".into(),
        exe.clone().into_os_string(),
        "one argument with spaces".into(),
    ];
    let r = parse(&args)?;
    assert_eq!(r.proton, proton);
    assert_eq!(r.executable, exe);
    assert_eq!(r.root, game);
    assert_eq!(r.arguments, ["one argument with spaces"]);
    Ok(())
}
