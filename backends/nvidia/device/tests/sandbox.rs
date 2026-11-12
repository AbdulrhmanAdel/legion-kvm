// The sandbox, checked from inside one.

#[test]
fn the_sandbox_refuses_what_it_says_it_refuses() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nvgpu-sandbox-selftest"))
        .output()
        .expect("run the selftest binary");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
