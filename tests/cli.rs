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
fn interactive_mode_requires_desktop_forwarding_and_excludes_a_session_timeout() {
    for extra in [
        vec![],
        vec![
            "--forward-display",
            "1920x1080",
            "--workload-timeout-seconds",
            "3600",
        ],
    ] {
        let directory = tempfile::tempdir().expect("private output parent");
        let output = directory.path().join("must-not-start");
        let result = legion()
            .args(["gpu-workload-probe", "--interactive", "true"])
            .args(extra)
            .arg("--output")
            .arg(&output)
            .output()
            .expect("invalid interactive command");
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("--interactive true requires --forward-display")
        );
        assert!(!output.exists(), "invalid options must not start a VM");
    }
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

#[test]
fn application_input_test_rejects_duration_too_short_for_pause_and_resume() {
    let directory = tempfile::tempdir().expect("test directory");
    let output = directory.path().join("output");
    let result = legion()
        .args([
            "gpu-application-probe",
            "--backend",
            "/bin/true",
            "--userspace",
            "/unused",
            "--virtiofsd",
            "/bin/true",
            "--guest-display",
            "/bin/true",
            "--presenter",
            "/bin/true",
            "--seconds",
            "1",
        ])
        .arg("--output")
        .arg(&output)
        .output()
        .expect("reject short application test");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("at least 3 seconds"));
    assert!(!output.exists());
}

#[test]
fn uncapped_mode_rejects_pausing_tests_and_non_application_probes() {
    let directory = tempfile::tempdir().expect("test directory");
    for (index, command, self_test, seconds, expected) in [
        (
            0,
            "gpu-application-probe",
            "true",
            "15",
            "uncapped benchmarks require",
        ),
        (
            1,
            "gpu-present-probe",
            "false",
            "15",
            "uncapped benchmarks require",
        ),
        (
            2,
            "gpu-application-probe",
            "false",
            "1",
            "at least 12 seconds",
        ),
    ] {
        let output = directory.path().join(index.to_string());
        let result = legion()
            .args([
                command,
                "--backend",
                "/bin/true",
                "--userspace",
                "/unused",
                "--virtiofsd",
                "/bin/true",
                "--guest-display",
                "/bin/true",
                "--presenter",
                "/bin/true",
                "--uncapped",
                "true",
                "--self-test",
                self_test,
                "--seconds",
                seconds,
            ])
            .arg("--output")
            .arg(&output)
            .output()
            .expect("reject invalid benchmark");
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected));
        assert!(!output.exists());
    }
}

#[test]
fn native_bridge_rejects_bad_affinity_before_creating_output_or_spawning() {
    let directory = tempfile::tempdir().expect("test directory");
    let output = directory.path().join("output");
    let result = legion()
        .args([
            "gpu-native-bridge-probe",
            "--userspace",
            "/unused",
            "--guest-display",
            "/bin/true",
            "--presenter",
            "/bin/true",
            "--native-cpus",
            "not-a-cpu-list",
        ])
        .arg("--output")
        .arg(&output)
        .output()
        .expect("reject invalid affinity");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("native CPU list"));
    assert!(!output.exists());
}

#[test]
fn native_forwarding_rejects_incomplete_logging_duration_before_spawning() {
    let directory = tempfile::tempdir().expect("test directory");
    let output = directory.path().join("output");
    let result = legion()
        .args([
            "gpu-native-forward-probe",
            "--compositor",
            "/bin/true",
            "--userspace",
            "/unused",
            "--seconds",
            "14",
        ])
        .arg("--output")
        .arg(&output)
        .output()
        .expect("invalid forwarding duration");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("15..=30"));
    assert!(!output.exists());
}

#[test]
fn vm_forwarding_rejects_unsupported_probe_modes_before_spawning() {
    let directory = tempfile::tempdir().expect("test directory");
    for (index, uncapped, self_test, seconds) in [
        (0, "false", "false", "15"),
        (1, "true", "true", "15"),
        (2, "true", "false", "12"),
    ] {
        let output = directory.path().join(index.to_string());
        let result = legion()
            .args([
                "gpu-application-probe",
                "--backend",
                "/bin/true",
                "--userspace",
                "/unused",
                "--virtiofsd",
                "/bin/true",
                "--guest-display",
                "/bin/true",
                "--presenter",
                "/bin/true",
                "--gpu-forward",
                "true",
                "--uncapped",
                uncapped,
                "--self-test",
                self_test,
                "--seconds",
                seconds,
            ])
            .arg("--output")
            .arg(&output)
            .output()
            .expect("reject unsupported forwarding mode");
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("GPU forwarding currently requires")
        );
        assert!(!output.exists());
    }
}

#[test]
fn acquire_mode_rejects_unknown_values_and_readback_before_spawning() {
    let directory = tempfile::tempdir().expect("test directory");
    for (mode, message) in [
        ("unknown", "acquire mode must be async or blocking"),
        ("async", "--acquire-mode requires --gpu-forward true"),
        ("blocking", "--acquire-mode requires --gpu-forward true"),
    ] {
        let output = directory.path().join(mode);
        let result = legion()
            .args([
                "gpu-application-probe",
                "--backend",
                "/bin/true",
                "--userspace",
                "/unused",
                "--virtiofsd",
                "/bin/true",
                "--guest-display",
                "/bin/true",
                "--presenter",
                "/bin/true",
                "--acquire-mode",
                mode,
            ])
            .arg("--output")
            .arg(&output)
            .output()
            .expect("reject acquire mode");
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(message));
        assert!(!output.exists());
    }
}

#[test]
fn frame_transport_rejects_unknown_values_and_readback_before_spawning() {
    let directory = tempfile::tempdir().expect("test directory");
    for (mode, message) in [
        (
            "unknown",
            "frame transport must be polled, notified, combined or separate",
        ),
        ("combined", "--frame-transport requires --gpu-forward true"),
        ("notified", "--frame-transport requires --gpu-forward true"),
        ("separate", "--frame-transport requires --gpu-forward true"),
    ] {
        let output = directory.path().join(mode);
        let result = legion()
            .args([
                "gpu-application-probe",
                "--backend",
                "/bin/true",
                "--userspace",
                "/unused",
                "--virtiofsd",
                "/bin/true",
                "--guest-display",
                "/bin/true",
                "--presenter",
                "/bin/true",
                "--frame-transport",
                mode,
            ])
            .arg("--output")
            .arg(&output)
            .output()
            .expect("reject frame transport");
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(message));
        assert!(!output.exists());
    }
}

#[test]
fn frame_pacing_rejects_unknown_values_and_readback_before_spawning() {
    let directory = tempfile::tempdir().expect("test directory");
    for (mode, message) in [
        ("unknown", "frame pacing must be callbacks or unpaced"),
        ("callbacks", "--frame-pacing requires --gpu-forward true"),
        ("unpaced", "--frame-pacing requires --gpu-forward true"),
    ] {
        let output = directory.path().join(mode);
        let result = legion()
            .args([
                "gpu-application-probe",
                "--backend",
                "/bin/true",
                "--userspace",
                "/unused",
                "--virtiofsd",
                "/bin/true",
                "--guest-display",
                "/bin/true",
                "--presenter",
                "/bin/true",
                "--frame-pacing",
                mode,
            ])
            .arg("--output")
            .arg(&output)
            .output()
            .expect("reject frame pacing");
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(message));
        assert!(!output.exists());
    }
}

#[test]
fn unsupported_gpu_mmio_layout_fails_before_spawning_or_creating_output() {
    let directory = tempfile::tempdir().expect("test directory");
    let output = directory.path().join("output");
    let result = legion()
        .args([
            "gpu-driver-probe",
            "--backend",
            "/bin/true",
            "--machine",
            "microvm",
        ])
        .arg("--output")
        .arg(&output)
        .output()
        .expect("reject MMIO GPU boot");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("512-byte virtio-MMIO window"));
    assert!(!output.exists());
}

#[test]
fn workload_configuration_is_rejected_before_creating_output_or_spawning_tools() {
    let directory = tempfile::tempdir().expect("test directory");
    let configuration = directory.path().join("configuration.json");
    let output = directory.path().join("output");
    // Valid JSON with a future schema must fail before accessing the ELF, kernel, backend, or QEMU.
    fs::write(
        &configuration,
        br#"{"schema":99,"warmup_ms":100,"sample_ms":500,"width":64,"height":64,"cases":[]}"#,
    )
    .expect("configuration fixture");
    for paired in [false, true] {
        let mut command = legion();
        command
            .args(["gpu-workload-probe", "--workload", "/must-not-be-read"])
            .arg("--output")
            .arg(&output)
            .args(["--backend", "/must-not-start", "--qemu", "/must-not-start"]);
        if paired {
            command.arg("--workload-config").arg(&configuration);
        }
        let result = command.output().expect("reject configuration");
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains(if paired {
                "unknown workload configuration schema"
            } else {
                "--workload and --workload-config are required"
            }),
            "{error}"
        );
        assert!(!output.exists());
    }
}
