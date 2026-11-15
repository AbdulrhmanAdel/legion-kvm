use super::*;

#[test]
fn window_retry_restarts_connected_filesystems_and_keeps_their_logs() -> Result<()> {
    use std::io::Write;
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let program = root.join("fake-virtiofsd");
    fs::write(
        &program,
        r#"#!/usr/bin/env python3
import socket, sys
path = sys.argv[sys.argv.index('--socket-path') + 1]
server = socket.socket(socket.AF_UNIX)
server.bind(path)
server.listen(1)
print('readonly=' + str('--readonly' in sys.argv), flush=True)
client, _ = server.accept()
client.recv(1)
"#,
    )?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    let mut command = Command::new("unused-qemu");
    let mut services = vec![share(
        &program,
        root,
        root,
        "usr",
        root,
        true,
        &mut command,
    )?];
    let original_args: Vec<_> = command.get_args().map(OsString::from).collect();
    let socket = root.join("usr.sock");
    let mut client = UnixStream::connect(&socket)?;
    client.write_all(b"x")?;
    services[0].wait_until(Duration::from_secs(5))?;
    restart_shares(&program, root, root, &[("usr", root, true)], &mut services)?;
    let mut client = UnixStream::connect(socket)?;
    client.write_all(b"x")?;
    services[0].wait_until(Duration::from_secs(5))?;
    assert!(fs::read_to_string(root.join("virtiofs-usr-wayland.log"))?.contains("readonly=True"));
    assert!(fs::read_to_string(root.join("virtiofs-usr.log"))?.contains("readonly=True"));
    assert_eq!(
        original_args,
        command.get_args().map(OsString::from).collect::<Vec<_>>()
    );
    Ok(())
}

#[test]
fn window_fallback_is_limited_to_initial_display_failures() {
    let mut config = Config {
        window: true,
        auto_window: true,
        ..Default::default()
    };
    let error = "qemu-system-x86_64: SDL window creation failed: Could not get EGL display";
    assert!(retry_window(&config, true, error, true));
    assert!(!retry_window(&config, false, error, true));
    assert!(!retry_window(&config, true, error, false));
    assert!(!retry_window(
        &config,
        true,
        "KVM initialization failed",
        true
    ));
    config.auto_window = false;
    assert!(!retry_window(&config, true, error, true));
    config.auto_window = true;
    config.x11_window = true;
    assert!(!retry_window(&config, true, error, true));
}

#[test]
fn serial_reports_wait_for_complete_lines() -> Result<()> {
    assert!(read_marker("LEGION_DRM_PROBE {\"passed\":tr", "LEGION_DRM_PROBE ")?.is_none());
    assert_eq!(
        read_marker(
            "boot\nLEGION_DRM_PROBE {\"passed\":true}\r\n",
            "LEGION_DRM_PROBE "
        )?,
        Some(json!({"passed":true}))
    );
    assert!(read_marker("LEGION_DRM_PROBE broken\n", "LEGION_DRM_PROBE ").is_err());
    Ok(())
}
