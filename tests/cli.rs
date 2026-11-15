use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use serde_json::Value;

fn legion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_legion"))
}

#[test]
fn plan_preserves_launcher_arguments_without_shell_evaluation() {
    let directory = tempfile::tempdir().expect("test directory");
    let args = [
        OsString::from("/custom Proton/proton"),
        OsString::from("waitforexitandrun"),
        OsString::from("game with spaces.exe"),
        OsString::from("$(touch unexpected-file); $HOME `id`"),
        OsString::from_vec(vec![b'f', 0xff]),
        OsString::new(),
        OsString::from("--machine"),
    ];
    let output = legion()
        .args(["plan", "--"])
        .args(&args)
        .current_dir(directory.path())
        .output()
        .expect("run plan");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("plan JSON");
    let actual: Vec<Vec<u8>> =
        serde_json::from_value(value["argv_bytes"].clone()).expect("argv bytes");
    let expected: Vec<Vec<u8>> = args.into_iter().map(OsString::into_vec).collect();
    assert_eq!(actual, expected);
    assert_eq!(value["game_launch_available"], false);
    assert_eq!(
        fs::read_dir(directory.path())
            .expect("read test directory")
            .count(),
        0
    );
}

#[test]
fn empty_or_ambiguous_commands_fail() {
    for args in [
        vec!["plan"],
        vec!["plan", "--"],
        vec!["plan", "proton", "game.exe"],
        vec!["cpu-probe", "--timeout", "0"],
        vec!["gpu-workload-probe", "--memory-mib", "0"],
        vec!["gpu-workload-probe", "--memory-mib", "65537"],
        vec!["gpu-workload-probe", "--memory-mib", "invalid"],
        vec!["gpu-workload-probe", "--qemu-hugepages", "invalid"],
        vec!["internal-qemu-exec"],
        vec!["internal-qemu-exec", "--"],
    ] {
        assert!(
            !legion()
                .args(args)
                .output()
                .expect("run invalid command")
                .status
                .success()
        );
    }
}

#[test]
fn qemu_exec_policy_survives_exec_without_changing_its_parent_or_arguments() {
    let before = rustix::thread::transparent_huge_pages_are_disabled().expect("parent policy");
    let directory = tempfile::tempdir().expect("private exec fixture");
    let script = directory.path().join("QEMU with spaces");
    fs::write(
        &script,
        b"#!/bin/sh\ngrep '^THP_enabled:' /proc/self/status\nprintf '%s\\n' \"$1\"\n",
    )
    .expect("exec fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable");
    let literal = "spaces; $(touch should-not-exist) `id`";
    let output = legion()
        .args(["internal-qemu-exec", "--"])
        .arg(&script)
        .arg(literal)
        .current_dir(directory.path())
        .output()
        .expect("child exec stage");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("fixture text");
    assert!(text.lines().any(|line| line == "THP_enabled:\t0"), "{text}");
    assert!(text.lines().any(|line| line == literal), "{text}");
    assert!(!directory.path().join("should-not-exist").exists());
    assert_eq!(
        rustix::thread::transparent_huge_pages_are_disabled().expect("parent remains readable"),
        before
    );
}

#[test]
fn invalid_guest_init_does_not_create_output_or_start_qemu() {
    let directory = tempfile::tempdir().expect("test directory");
    let kernel = directory.path().join("kernel");
    let init = directory.path().join("init");
    let output = directory.path().join("output");
    fs::write(&kernel, b"kernel fixture").expect("kernel fixture");
    fs::write(&init, b"not an ELF").expect("init fixture");
    let result = legion()
        .arg("cpu-probe")
        .arg("--kernel")
        .arg(&kernel)
        .arg("--init")
        .arg(&init)
        .arg("--output")
        .arg(&output)
        .args(["--qemu", "/this-program-must-not-run"])
        .output()
        .expect("reject invalid init");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("64-bit little-endian ELF"));
    assert!(!output.exists());
}

#[test]
fn timeout_reaps_only_the_spawned_vm_and_keeps_failure_evidence() {
    let directory = tempfile::tempdir().expect("test directory");
    let kernel = directory.path().join("kernel");
    let init = directory.path().join("init");
    let output = directory.path().join("output");
    let qemu = directory.path().join("fake-qemu");
    let pid_path = directory.path().join("child.pid");
    let args_path = directory.path().join("child.args");
    fs::write(&kernel, b"kernel fixture").expect("kernel fixture");
    // A structurally valid static ELF header, for the launch-boundary test.
    let mut elf = vec![0_u8; 120];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
    elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
    elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1_u16.to_le_bytes());
    fs::write(&init, elf).expect("ELF fixture");
    fs::write(
        &qemu,
        b"#!/bin/sh\nprintf '%s' \"$$\" > \"$LEGION_TEST_PID_PATH\"\nprintf '%s\\n' \"$@\" > \"$LEGION_TEST_ARGS_PATH\"\nexec /bin/sleep 30\n",
    )
    .expect("fake QEMU");
    fs::set_permissions(&qemu, fs::Permissions::from_mode(0o700)).expect("executable fixture");
    let result = legion()
        .arg("cpu-probe")
        .arg("--kernel")
        .arg(&kernel)
        .arg("--init")
        .arg(&init)
        .arg("--output")
        .arg(&output)
        .arg("--qemu")
        .arg(&qemu)
        .args(["--timeout", "1"])
        .env("LEGION_TEST_PID_PATH", &pid_path)
        .env("LEGION_TEST_ARGS_PATH", &args_path)
        .output()
        .expect("timeout probe");
    assert!(!result.status.success());
    let report: Value =
        serde_json::from_slice(&fs::read(output.join("report.json")).expect("report"))
            .expect("report JSON");
    assert_eq!(report["passed"], false);
    let topology = &report["cpu_topology"];
    let args = fs::read_to_string(args_path).expect("QEMU arguments");
    let args = args.lines().collect::<Vec<_>>();
    let smp = args
        .windows(2)
        .find(|args| args[0] == "-smp")
        .expect("explicit SMP argument")[1];
    let topology: legion_cpu_topology::Topology =
        serde_json::from_value(topology.clone()).expect("reported topology");
    assert_eq!(smp, topology.qemu_smp().expect("QEMU layout"));
    assert!(
        report["error"]
            .as_str()
            .expect("error")
            .contains("timed out")
    );
    let pid = fs::read_to_string(pid_path).expect("child PID");
    assert!(
        !std::path::Path::new("/proc").join(pid).exists(),
        "owned VM must be reaped"
    );
}
