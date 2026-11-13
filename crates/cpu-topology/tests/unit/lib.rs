use super::*;

fn uniform(sockets: u32, cores: u32, threads: u32) -> Topology {
    let records = (0..sockets)
        .flat_map(|socket| {
            (0..cores).flat_map(move |core| {
                (0..threads).map(move |thread| {
                    ((socket * cores + core) * threads + thread, socket, 0, core)
                })
            })
        })
        .collect::<Vec<_>>();
    Topology::from_records(&records).expect("uniform topology")
}

#[test]
fn mirrors_core_counts_with_and_without_smt() {
    for (cores, threads) in [(8, 2), (6, 2), (4, 2), (4, 1), (1, 1)] {
        let topology = uniform(1, cores, threads);
        assert_eq!(topology.logical_cpus, cores * threads);
        assert_eq!(topology.physical_cores, cores);
        assert_eq!(
            topology.qemu_smp().expect("QEMU layout"),
            format!(
                "cpus={},sockets=1,cores={cores},threads={threads}",
                cores * threads
            )
        );
    }
}

#[test]
fn package_and_die_ids_scope_physical_cores() {
    let topology = Topology::from_records(&[
        (2, 7, 0, 0),
        (6, 7, 0, 0),
        (10, 7, 1, 0),
        (14, 7, 1, 0),
        (20, 9, 0, 0),
        (24, 9, 0, 0),
        (28, 9, 1, 0),
        (32, 9, 1, 0),
    ])
    .expect("sparse CPU IDs across packages/dies");
    assert_eq!(
        topology.qemu_smp().expect("QEMU layout"),
        "cpus=8,sockets=2,cores=2,threads=2"
    );
    assert!(topology.same_shape(&uniform(2, 2, 2)));
}

#[test]
fn rejects_unequal_sockets_and_invalid_records() {
    for records in [
        vec![(0, 0, 0, 0), (1, 1, 0, 0), (2, 1, 0, 1)],
        vec![],
        vec![(0, 0, 0, 0), (0, 0, 0, 1)],
    ] {
        assert!(Topology::from_records(&records).is_err());
    }
}

#[test]
fn mixed_smt_preserves_core_siblings_with_empty_qemu_slots() -> Result<()> {
    let topology = Topology::from_records(&[
        (0, 0, 0, 0),
        (8, 0, 0, 0),
        (1, 0, 0, 1),
        (9, 0, 0, 1),
        (2, 0, 0, 2),
        (3, 0, 0, 3),
    ])?;
    assert_eq!((topology.physical_cores, topology.logical_cpus), (4, 6));
    assert!(!topology.uniform_smt());
    let args = topology.qemu_args("host,-avx512f")?;
    assert_eq!(args[1], "cpus=1,sockets=1,cores=4,threads=2,maxcpus=8");
    let devices = args[2..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            assert_eq!(pair[0], "-device");
            pair[1].as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        devices,
        [
            "host-x86_64-cpu,socket-id=0,core-id=0,thread-id=1",
            "host-x86_64-cpu,socket-id=0,core-id=1,thread-id=0",
            "host-x86_64-cpu,socket-id=0,core-id=1,thread-id=1",
            "host-x86_64-cpu,socket-id=0,core-id=2,thread-id=0",
            "host-x86_64-cpu,socket-id=0,core-id=3,thread-id=0",
        ]
    );
    let other = Topology::from_records(&[
        (12, 5, 0, 7),
        (13, 5, 0, 8),
        (14, 5, 0, 9),
        (15, 5, 0, 9),
        (16, 5, 1, 0),
        (17, 5, 1, 0),
    ])?;
    assert!(topology.same_shape(&other));
    let mut invalid = topology.clone();
    invalid.cores.clear();
    assert!(!topology.same_shape(&invalid));
    assert!(invalid.qemu_args("host").is_err());
    let mut invalid = topology.clone();
    invalid.cores[0].cpu_ids[0] = invalid.cores[1].cpu_ids[0];
    assert!(!topology.same_shape(&invalid));
    assert!(invalid.qemu_args("host").is_err());
    Ok(())
}

#[test]
fn core_distribution_is_checked_beyond_summary_counts() -> Result<()> {
    let a = Topology::from_records(&[
        (0, 0, 0, 0),
        (1, 0, 0, 0),
        (2, 0, 0, 0),
        (3, 0, 0, 1),
        (4, 0, 0, 1),
        (5, 0, 0, 1),
        (6, 0, 0, 2),
        (7, 0, 0, 3),
    ])?;
    let b = Topology::from_records(&[
        (0, 0, 0, 0),
        (1, 0, 0, 0),
        (2, 0, 0, 0),
        (3, 0, 0, 1),
        (4, 0, 0, 1),
        (5, 0, 0, 2),
        (6, 0, 0, 2),
        (7, 0, 0, 3),
    ])?;
    assert_eq!(a.logical_cpus, b.logical_cpus);
    assert_eq!(a.threads_per_core, b.threads_per_core);
    assert!(!a.same_shape(&b));
    let uniform = uniform(1, 4, 2);
    let mut legacy = uniform.clone();
    legacy.cores.clear();
    assert!(uniform.same_shape(&legacy));
    assert_eq!(
        uniform.qemu_args("host")?,
        ["-smp", "cpus=8,sockets=1,cores=4,threads=2"]
    );
    Ok(())
}

#[test]
fn cpu_lists_validate_ranges_and_affinity() {
    assert_eq!(
        parse_cpu_list("0-2,5,9-10\n").expect("CPU list"),
        [0, 1, 2, 5, 9, 10]
    );
    for text in ["", "2-0", "0,0", "0-2,2-3", "1-2-3", "a", "0-4294967295"] {
        assert!(parse_cpu_list(text).is_err(), "{text}");
    }
    let topology = uniform(1, 4, 2);
    assert!(topology.check_affinity("0-7").is_ok());
    assert!(topology.check_affinity("0-3").is_err());
    assert!(!topology.same_shape(&uniform(1, 8, 1)));
}

#[test]
fn sysfs_uses_online_cpus_and_handles_older_missing_die_id() {
    let root = tempfile::tempdir().expect("sysfs fixture");
    fs::write(root.path().join("online"), "2,6\n").expect("online CPUs");
    for id in [2, 6] {
        let directory = root.path().join(format!("cpu{id}/topology"));
        fs::create_dir_all(&directory).expect("topology directory");
        fs::write(directory.join("physical_package_id"), "3\n").expect("package");
        fs::write(directory.join("core_id"), "11\n").expect("core");
    }
    let topology = Topology::read_sysfs(root.path()).expect("read online topology");
    assert_eq!(topology.cpu_ids, [2, 6]);
    assert!(topology.same_shape(&uniform(1, 1, 2)));
}
