use super::*;

#[test]
#[ignore = "requires LEGION_TEST_QEMU, LEGION_TEST_KERNEL, LEGION_TEST_INIT and accessible KVM"]
fn mixed_smt_layout_boots_and_matches_guest_online_siblings() -> Result<()> {
    use legion_cpu_topology::Core;
    let temporary = tempfile::tempdir()?;
    let records = Topology {
        logical_cpus: 6,
        physical_cores: 4,
        sockets: 1,
        cores_per_socket: 4,
        threads_per_core: 2,
        cpu_ids: (0..6).collect(),
        cores: vec![
            Core {
                package: 0,
                die: 0,
                core: 0,
                cpu_ids: vec![0, 1],
            },
            Core {
                package: 0,
                die: 0,
                core: 1,
                cpu_ids: vec![2, 3],
            },
            Core {
                package: 0,
                die: 0,
                core: 2,
                cpu_ids: vec![4],
            },
            Core {
                package: 0,
                die: 0,
                core: 3,
                cpu_ids: vec![5],
            },
        ],
    };
    let env_path = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .with_context(|| format!("set {name}"))
    };
    let mut single_first = records.clone();
    for core in &mut single_first.cores {
        core.core = 3 - core.core;
    }
    let mut reports = Vec::new();
    for (name, topology) in [
        ("smt-first", records),
        ("single-first", single_first),
        ("host-control", cpu::host_topology()?),
    ] {
        let report = run_with_topology(
            &Config {
                qemu: env_path("LEGION_TEST_QEMU")?,
                kernel: env_path("LEGION_TEST_KERNEL")?,
                init: env_path("LEGION_TEST_INIT")?,
                output: temporary.path().join(name),
                machine: Machine::Q35,
                timeout: Duration::from_secs(30),
            },
            || Ok(topology),
        )?;
        let passed = report.passed;
        reports.push(serde_json::json!({"case":name,"report":report}));
        if let Some(destination) = std::env::var_os("LEGION_TEST_REPORT") {
            fs::write(destination, serde_json::to_vec_pretty(&reports)?)?;
        }
        ensure!(passed, "guest layout {name} failed: {reports:?}");
    }
    Ok(())
}

#[test]
fn guest_verification_checks_cores_and_threads_not_just_vcpu_count() {
    let expected = Topology {
        logical_cpus: 16,
        physical_cores: 8,
        sockets: 1,
        cores_per_socket: 8,
        threads_per_core: 2,
        cpu_ids: (0..16).collect(),
        cores: Vec::new(),
    };
    let mut report = GuestReport {
        schema: 2,
        passed: true,
        xcr0: Some(7),
        avx512_sigill: true,
        leaf0: [0; 4],
        leaf1: [0; 4],
        leaf7: [0; 4],
        leafd0: [7, 0, 0, 0],
        topology: Some(expected.clone()),
        topology_error: None,
    };
    assert!(report.verify(&expected).is_ok());
    report.topology = Some(Topology {
        physical_cores: 16,
        cores_per_socket: 16,
        threads_per_core: 1,
        ..expected.clone()
    });
    assert!(report.verify(&expected).is_err());
    report.topology = None;
    assert!(report.verify(&expected).is_err());
}
