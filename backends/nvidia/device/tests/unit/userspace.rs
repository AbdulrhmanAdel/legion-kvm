use super::{Entry, FileKind, Resolved, loaded_driver_version, staged_driver_version, version_in};
use std::collections::BTreeSet as TestSet;

fn resolved(host_name: &str) -> Resolved {
    Resolved {
        entry: Entry {
            name: host_name.to_string(),
            kind: FileKind::Lib,
            categories: TestSet::new(),
        },
        host_path: std::path::PathBuf::from("/usr/lib/x86_64-linux-gnu").join(host_name),
        guest_path: std::path::PathBuf::from("lib").join(host_name),
    }
}

#[test]
fn retargeting_swaps_the_build_and_keeps_the_categories() {
    let mut cats = TestSet::new();
    cats.insert("compute".to_string());
    let entries = vec![
        Entry {
            name: "libcuda.so.595.91.07".into(),
            kind: FileKind::Lib,
            categories: cats.clone(),
        },
        // Unversioned on disk, so it must survive untouched.
        Entry {
            name: "nvidia_icd.json".into(),
            kind: FileKind::Json,
            categories: cats.clone(),
        },
    ];
    let out = super::retarget(&entries, "595.91.07", "595.99.02");
    assert_eq!(out[0].name, "libcuda.so.595.99.02");
    assert_eq!(out[0].categories, cats, "capability must survive the swap");
    assert_eq!(
        out[1].name, "nvidia_icd.json",
        "an unversioned name is not rewritten"
    );
}

#[test]
fn version_is_read_from_the_proc_line_the_module_writes() {
    let dir = std::env::temp_dir().join(format!("nvgpu-ver-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("version");
    std::fs::write(
        &p,
        "NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.99.02  Wed Aug 26 05:24:08 UTC 2026\n\
             GCC version:  gcc version 15.2.0 (Ubuntu 15.2.0-16ubuntu1)\n",
    )
    .unwrap();
    assert_eq!(loaded_driver_version(&p).as_deref(), Some("595.99.02"));
    // A host with no driver loaded has no such file, and that is not an error here -- planning a share off-box is a legitimate use.
    assert_eq!(loaded_driver_version(&dir.join("absent")), None);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_gcc_version_on_the_second_line_is_not_mistaken_for_the_driver() {
    // "gcc version 15.2.0" is dotted and comes second; the driver version is on the first line, so a find_map over lines must take that one.
    assert_eq!(
        version_in("GCC version:  gcc version 15.2.0 (Ubuntu)"),
        Some("15.2.0".into())
    );
    assert_eq!(
        version_in("libcuda.so.1"),
        None,
        "so.1 is not a driver version"
    );
    assert_eq!(version_in("libcuda.so.595.99.02"), Some("595.99.02".into()));
    assert_eq!(version_in("no digits here"), None);
}

#[test]
fn the_staged_version_is_the_one_most_files_carry() {
    // The shape that caused this check to exist: a host carrying two driver userspaces, where the manifest described the packaged one.
    let found = vec![
        resolved("libcuda.so.595.91.07"),
        resolved("libnvidia-glcore.so.595.91.07"),
        resolved("libnvidia-eglcore.so.595.91.07"),
        resolved("libnvidia-sandboxutils.so.595.99.02"),
        resolved("libnvidia-egl-wayland.so.1.1.21"),
    ];
    assert_eq!(staged_driver_version(&found).as_deref(), Some("595.91.07"));
    assert_eq!(staged_driver_version(&[]), None);
}

use super::*;

/// A sample in the driver's format, written here rather than copied from a driver package.
const SAMPLE: &str = r#"[
        {"name": "firmware/gsp_ga10x.bin", "type": "FIRMWARE", "category": ["firmware"]},
        {"name": "nvidia-modprobe", "type": "MODPROBE", "category": ["modprobe"]},
        {"name": "nvidia-smi", "type": "BINARY", "category": ["nvml"]},
        {"name": "libnvidia-ml.so.615.71.09", "type": "LIB", "category": ["nvml"]},
        {"name": "libcuda.so.615.71.09", "type": "LIB", "category": ["cuda", "optix", "vulkan"]},
        {"name": "libnvidia-encode.so.615.71.09", "type": "LIB", "category": ["video"]},
        {"name": "libnvidia-eglcore.so.615.71.09", "type": "LIB", "category": ["egl", "egl_headless"]},
        {"name": "nvidia_icd.json", "type": "JSON", "category": ["vulkan"]},
        {"name": "libnvidia-fbc.so.615.71.09", "type": "LIB", "category": ["xdriver"], "optional": true}
    ]"#;

fn sample() -> Vec<Entry> {
    parse_manifest(SAMPLE).expect("the sample parses")
}

#[test]
fn parses_every_entry_with_its_kind_and_categories() {
    let e = sample();
    assert_eq!(e.len(), 9);
    assert_eq!(e[2].name, "nvidia-smi");
    assert_eq!(e[2].kind, FileKind::Binary);
    assert!(e[4].categories.contains("cuda"));
    assert!(e[4].categories.contains("vulkan"));
}

/// The `optional` key is a bool this parser does not model.
#[test]
fn an_unmodelled_field_does_not_lose_the_entry() {
    let e = sample();
    let last = e.last().expect("a last entry");
    assert_eq!(last.name, "libnvidia-fbc.so.615.71.09");
    assert_eq!(last.kind, FileKind::Lib);
}

#[test]
fn firmware_and_modprobe_are_host_only() {
    let e = sample();
    assert!(e[0].is_host_only(), "GSP firmware must not reach a guest");
    assert!(
        e[1].is_host_only(),
        "nvidia-modprobe must not reach a guest"
    );
    assert!(!e[2].is_host_only());
}

#[test]
fn capabilities_select_disjoint_sets() {
    let e = sample();
    let names = |caps: &[Capability]| -> Vec<&str> {
        e.iter()
            .filter(|x| !x.is_host_only() && x.wanted_by(caps))
            .map(|x| x.name.as_str())
            .collect()
    };
    assert_eq!(
        names(&[Capability::Utility]),
        ["nvidia-smi", "libnvidia-ml.so.615.71.09"]
    );
    assert_eq!(
        names(&[Capability::Video]),
        ["libnvidia-encode.so.615.71.09"]
    );
    assert!(names(&[Capability::Compute]).contains(&"libcuda.so.615.71.09"));
}

/// `libcuda` is categorised for cuda *and* vulkan.
#[test]
fn a_file_in_two_categories_resolves_once() {
    let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
    let (found, _) = resolve(
        &sample(),
        &[Capability::Compute, Capability::Graphics],
        &[dir.path().join("lib")],
    );
    let cuda: Vec<_> = found
        .iter()
        .filter(|r| r.entry.name.starts_with("libcuda"))
        .collect();
    assert_eq!(cuda.len(), 1, "libcuda was staged {} times", cuda.len());
}

#[test]
fn a_json_keeps_the_directory_tail_its_loader_looks_in() {
    let dir = TempTree::new(&["share/vulkan/icd.d/nvidia_icd.json"]);
    let (found, _) = resolve(
        &sample(),
        &[Capability::Graphics],
        &[dir.path().join("share/vulkan/icd.d")],
    );
    let icd = found
        .iter()
        .find(|r| r.entry.name == "nvidia_icd.json")
        .expect("the ICD resolved");
    assert!(
        icd.guest_path.ends_with("vulkan/icd.d/nvidia_icd.json"),
        "an ICD landed at {:?}, where the Vulkan loader will not look",
        icd.guest_path
    );
}

/// How a real manifest repeats itself: the same file appears once per group of capabilities it serves, as separate entries rather than one entry with several categories.
const SAMPLE_WITH_REPEATS: &str = r#"[
        {"name": "libcuda.so.615.71.09", "type": "LIB", "category": ["cuda"]},
        {"name": "libcuda.so.615.71.09", "type": "LIB", "category": ["vulkan"]},
        {"name": "libnvidia-opencl.so.615.71.09", "type": "LIB", "category": ["opencl"]},
        {"name": "libnvidia-opencl.so.615.71.09", "type": "LIB", "category": ["cuda"]}
    ]"#;

/// Staging the same path twice fails with EEXIST and leaves the share built only as far as the first duplicate, so a repeated entry must resolve once.
#[test]
fn a_file_listed_twice_is_staged_once() {
    let entries = parse_manifest(SAMPLE_WITH_REPEATS).expect("parses");
    assert_eq!(entries.len(), 4, "the manifest really does repeat entries");

    let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
    let (found, missing) = resolve(
        &entries,
        &[Capability::Compute, Capability::Graphics],
        &[dir.path().join("lib")],
    );

    assert_eq!(found.len(), 1, "libcuda staged {} times", found.len());
    assert_eq!(
        missing.len(),
        1,
        "the absent file was reported {} times",
        missing.len()
    );
}

/// The categories of a repeated entry belong to the one that is kept -- otherwise the record says libcuda serves compute but not graphics, purely because of which duplicate came first.
#[test]
fn a_repeated_entry_keeps_every_category_it_was_listed_under() {
    let entries = parse_manifest(SAMPLE_WITH_REPEATS).expect("parses");
    let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
    let (found, _) = resolve(
        &entries,
        &[Capability::Compute, Capability::Graphics],
        &[dir.path().join("lib")],
    );
    let cuda = &found[0].entry;
    assert!(cuda.categories.contains("cuda"));
    assert!(
        cuda.categories.contains("vulkan"),
        "categories were {:?}",
        cuda.categories
    );
}

#[test]
fn what_is_absent_is_reported_rather_than_failing() {
    let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
    let (found, missing) = resolve(
        &sample(),
        &[Capability::Compute, Capability::Video],
        &[dir.path().join("lib")],
    );
    assert_eq!(found.len(), 1);
    assert!(
        missing
            .iter()
            .any(|e| e.name.starts_with("libnvidia-encode")),
        "an absent file should be reported, not silently dropped"
    );
}

/// A minimal but structurally valid ELF64 shared object carrying one SONAME.
fn synthetic_so(soname: &str) -> Vec<u8> {
    const VADDR_BASE: u64 = 0x1000;
    let ehdr_len = 64usize;
    let phdr_len = 56usize;
    let dyn_off = (ehdr_len + phdr_len * 2) as u64;
    let dyn_len = 48u64; // three 16-byte entries
    let strtab_off = dyn_off + dyn_len;
    // index 0 is the customary empty string
    let strtab = {
        let mut v = vec![0u8];
        v.extend_from_slice(soname.as_bytes());
        v.push(0);
        v
    };

    let mut f = vec![0u8; (strtab_off as usize) + strtab.len()];
    f[0..4].copy_from_slice(b"\x7fELF");
    f[4] = 2; // ELF64
    f[5] = 1; // little endian
    f[6] = 1; // version
    f[0x20..0x28].copy_from_slice(&(ehdr_len as u64).to_le_bytes()); // e_phoff
    f[0x36..0x38].copy_from_slice(&(phdr_len as u16).to_le_bytes()); // e_phentsize
    f[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

    let total = f.len() as u64;
    // PT_LOAD covering the whole file, mapped at VADDR_BASE.
    let p0 = ehdr_len;
    f[p0..p0 + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    f[p0 + 8..p0 + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    f[p0 + 16..p0 + 24].copy_from_slice(&VADDR_BASE.to_le_bytes()); // p_vaddr
    f[p0 + 32..p0 + 40].copy_from_slice(&total.to_le_bytes()); // p_filesz

    // PT_DYNAMIC
    let p1 = ehdr_len + phdr_len;
    f[p1..p1 + 4].copy_from_slice(&2u32.to_le_bytes()); // PT_DYNAMIC
    f[p1 + 8..p1 + 16].copy_from_slice(&dyn_off.to_le_bytes());
    f[p1 + 32..p1 + 40].copy_from_slice(&dyn_len.to_le_bytes());

    let mut put = |i: usize, tag: i64, val: u64| {
        let o = dyn_off as usize + i * 16;
        f[o..o + 8].copy_from_slice(&tag.to_le_bytes());
        f[o + 8..o + 16].copy_from_slice(&val.to_le_bytes());
    };
    put(0, 14, 1); // DT_SONAME -> strtab index 1
    put(1, 5, VADDR_BASE + strtab_off); // DT_STRTAB, as a vaddr
    put(2, 0, 0); // DT_NULL

    f[strtab_off as usize..].copy_from_slice(&strtab);
    f
}

#[test]
fn soname_is_read_from_the_elf_not_guessed_from_the_filename() {
    let dir = TempTree::new(&[]);
    let path = dir.path().join("libnvidia-nvvm70.so.615.71.09");
    std::fs::create_dir_all(dir.path()).expect("mkdir");
    std::fs::write(&path, synthetic_so("libnvidia-nvvm70.so.4")).expect("write");

    // The filename says 615.71.09 and the SONAME says 4.
    assert_eq!(soname(&path).as_deref(), Some("libnvidia-nvvm70.so.4"));
}

#[test]
fn a_file_with_no_soname_is_not_an_error() {
    let dir = TempTree::new(&["notelf"]);
    assert_eq!(soname(&dir.path().join("notelf")), None);
    assert_eq!(soname(Path::new("/nonexistent/at/all")), None);
}

#[test]
fn a_manifest_that_is_not_an_array_is_rejected() {
    assert!(parse_manifest("{}").is_err());
    assert!(
        parse_manifest("[{\"type\": \"LIB\"}]").is_err(),
        "an entry with no name"
    );
}

/// A directory of empty files, removed on drop.
struct TempTree(PathBuf);

impl TempTree {
    fn new(files: &[&str]) -> Self {
        let base = std::env::temp_dir().join(format!(
            "nvgpu-userspace-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir base");
        for f in files {
            let p = base.join(f);
            std::fs::create_dir_all(p.parent().expect("a parent")).expect("mkdir");
            std::fs::write(&p, b"").expect("write");
        }
        Self(base)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
