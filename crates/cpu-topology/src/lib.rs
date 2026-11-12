//! Linux online socket/core/SMT topology, shared by the launcher and guest.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// Online socket/core/SMT layout, including mixed-SMT CPUs.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Topology {
    pub logical_cpus: u32,
    pub physical_cores: u32,
    pub sockets: u32,
    pub cores_per_socket: u32,
    /// Maximum online siblings on a core; consult `cores` for mixed SMT.
    pub threads_per_core: u32,
    pub cpu_ids: Vec<u32>,
    /// Older uniform reports omitted this detail.
    #[serde(default)]
    pub cores: Vec<Core>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Core {
    pub package: u32,
    pub die: u32,
    pub core: u32,
    pub cpu_ids: Vec<u32>,
}

impl Topology {
    /// Detect online topology from the running Linux kernel. # Errors Returns an error for unavailable/inconsistent sysfs data, CPU hotplug during detection, or a layout with unequal socket core counts.
    pub fn detect() -> Result<Self> {
        Self::read_sysfs(Path::new("/sys/devices/system/cpu"))
    }

    fn read_sysfs(root: &Path) -> Result<Self> {
        let online = fs::read_to_string(root.join("online")).context("read online CPU list")?;
        let mut records = Vec::new();
        for id in parse_cpu_list(&online)? {
            let directory = root.join(format!("cpu{id}/topology"));
            let read_id = |name| -> Result<u32> {
                let path = directory.join(name);
                fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid CPU topology ID in {}", path.display()))
            };
            // Older x86 kernels may omit die_id.
            let die = if directory.join("die_id").exists() {
                read_id("die_id")?
            } else {
                0
            };
            records.push((
                id,
                read_id("physical_package_id")?,
                die,
                read_id("core_id")?,
            ));
        }
        ensure!(
            parse_cpu_list(&fs::read_to_string(root.join("online"))?)?
                == records.iter().map(|record| record.0).collect::<Vec<_>>(),
            "online CPUs changed during topology detection; retry the launch"
        );
        Self::from_records(&records)
    }

    fn from_records(records: &[(u32, u32, u32, u32)]) -> Result<Self> {
        ensure!(!records.is_empty(), "no online CPUs");
        let mut cores: BTreeMap<(u32, u32, u32), Vec<u32>> = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for &(id, package, die, core) in records {
            ensure!(ids.insert(id), "duplicate logical CPU ID {id}");
            cores.entry((package, die, core)).or_default().push(id);
        }
        let threads = cores
            .values()
            .map(Vec::len)
            .max()
            .context("no physical cores")?;
        let mut packages = BTreeMap::<u32, usize>::new();
        for &(package, _, _) in cores.keys() {
            *packages.entry(package).or_default() += 1;
        }
        let per_socket = *packages.values().next().context("no CPU packages")?;
        ensure!(
            packages.values().all(|count| *count == per_socket),
            "unequal cores per socket cannot yet be mirrored by Legion"
        );
        Ok(Self {
            logical_cpus: u32::try_from(ids.len())?,
            physical_cores: u32::try_from(cores.len())?,
            sockets: u32::try_from(packages.len())?,
            cores_per_socket: u32::try_from(per_socket)?,
            threads_per_core: u32::try_from(threads)?,
            cpu_ids: ids.into_iter().collect(),
            cores: cores
                .into_iter()
                .map(|((package, die, core), mut cpu_ids)| {
                    cpu_ids.sort_unstable();
                    Core {
                        package,
                        die,
                        core,
                        cpu_ids,
                    }
                })
                .collect(),
        })
    }

    pub fn uniform_smt(&self) -> bool {
        self.physical_cores.checked_mul(self.threads_per_core) == Some(self.logical_cpus)
            && self
                .cores
                .iter()
                .all(|core| core.cpu_ids.len() == self.threads_per_core as usize)
    }

    /// Slot capacity; mixed SMT leaves unused sibling slots absent.
    pub fn qemu_slots(&self) -> Result<u32> {
        self.sockets
            .checked_mul(self.cores_per_socket)
            .and_then(|cores| cores.checked_mul(self.threads_per_core))
            .filter(|slots| *slots > 0)
            .context("invalid or overflowing QEMU CPU slot capacity")
    }

    /// Mixed layouts need the complete `qemu_args`, not only `-smp`.
    pub fn qemu_smp(&self) -> Result<String> {
        ensure!(self.valid_shape(), "invalid CPU topology records");
        let slots = self.qemu_slots()?;
        if self.uniform_smt() {
            return Ok(format!(
                "cpus={},sockets={},cores={},threads={}",
                self.logical_cpus, self.sockets, self.cores_per_socket, self.threads_per_core
            ));
        }
        Ok(format!(
            "cpus=1,sockets={},cores={},threads={},maxcpus={slots}",
            self.sockets, self.cores_per_socket, self.threads_per_core,
        ))
    }

    /// Cold-plug only real online siblings into QEMU's topology slots.
    pub fn qemu_args(&self, model: &str) -> Result<Vec<String>> {
        let mut args = vec!["-smp".to_owned(), self.qemu_smp()?];
        if self.uniform_smt() {
            return Ok(args);
        }
        let model = model.split(',').next().context("missing QEMU CPU model")?;
        ensure!(
            !model.is_empty()
                && model
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
            "invalid QEMU CPU model"
        );
        let mut packages = BTreeMap::<u32, Vec<&Core>>::new();
        for core in &self.cores {
            packages.entry(core.package).or_default().push(core);
        }
        for (socket, cores) in packages.values_mut().enumerate() {
            cores.sort_by_key(|core| (core.die, core.core));
            for (core, record) in cores.iter().enumerate() {
                for thread in 0..record.cpu_ids.len() {
                    if (socket, core, thread) == (0, 0, 0) {
                        continue;
                    }
                    args.push("-device".into());
                    args.push(format!(
                        "{model}-x86_64-cpu,socket-id={socket},core-id={core},thread-id={thread}"
                    ));
                }
            }
        }
        Ok(args)
    }

    fn valid_shape(&self) -> bool {
        if self.logical_cpus == 0
            || self.physical_cores == 0
            || self.threads_per_core == 0
            || self.sockets.checked_mul(self.cores_per_socket) != Some(self.physical_cores)
            || self.cpu_ids.len() != self.logical_cpus as usize
            || self.cpu_ids.windows(2).any(|ids| ids[0] >= ids[1])
        {
            return false;
        }
        if self.cores.is_empty() {
            return self.uniform_smt();
        }
        if self.cores.len() != self.physical_cores as usize {
            return false;
        }
        let mut packages = BTreeMap::<u32, usize>::new();
        let mut keys = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut maximum = 0;
        for core in &self.cores {
            if core.cpu_ids.is_empty() || !keys.insert((core.package, core.die, core.core)) {
                return false;
            }
            for &id in &core.cpu_ids {
                if !ids.insert(id) {
                    return false;
                }
            }
            maximum = maximum.max(core.cpu_ids.len());
            *packages.entry(core.package).or_default() += 1;
        }
        maximum == self.threads_per_core as usize
            && packages.len() == self.sockets as usize
            && packages
                .values()
                .all(|count| *count == self.cores_per_socket as usize)
            && ids.into_iter().eq(self.cpu_ids.iter().copied())
    }

    fn shape(&self) -> Vec<Vec<usize>> {
        let mut packages = BTreeMap::<u32, Vec<usize>>::new();
        for core in &self.cores {
            packages
                .entry(core.package)
                .or_default()
                .push(core.cpu_ids.len());
        }
        let mut shape = packages.into_values().collect::<Vec<_>>();
        for cores in &mut shape {
            cores.sort_unstable();
        }
        shape.sort_unstable();
        shape
    }

    /// Compare core/thread counts, allowing Linux to renumber CPU/package IDs.
    pub fn same_shape(&self, other: &Self) -> bool {
        let same = self.logical_cpus == other.logical_cpus
            && self.physical_cores == other.physical_cores
            && self.sockets == other.sockets
            && self.cores_per_socket == other.cores_per_socket
            && self.threads_per_core == other.threads_per_core;
        same && self.valid_shape()
            && other.valid_shape()
            && if self.cores.is_empty() || other.cores.is_empty() {
                self.uniform_smt() && other.uniform_smt()
            } else {
                self.shape() == other.shape()
            }
    }

    /// Check that a launcher can execute on every CPU it intends to mirror. # Errors Rejects invalid affinity lists and restricted access to online host CPUs.
    pub fn check_affinity(&self, allowed: &str) -> Result<()> {
        let allowed = parse_cpu_list(allowed)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        ensure!(
            self.cpu_ids.iter().all(|id| allowed.contains(id)),
            "launcher CPU affinity/cpuset excludes online host CPUs; cannot mirror the full host topology within this restriction"
        );
        Ok(())
    }
}

fn parse_cpu_list(text: &str) -> Result<Vec<u32>> {
    let mut ids = BTreeSet::new();
    for part in text.trim().split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let first: u32 = first.parse().context("invalid CPU list start")?;
        let last: u32 = last.parse().context("invalid CPU list end")?;

        ensure!(first <= last && last < 1_048_576, "invalid CPU list range");
        for id in first..=last {
            ensure!(ids.insert(id), "duplicate CPU ID {id}");
        }
    }
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
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
}
