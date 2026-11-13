//! Which host files a guest needs in order to drive the forwarded GPU.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The driver's own manifest, on a host where one is installed.
pub const DEFAULT_MANIFEST: &str = "/usr/share/nvidia/files.d/sandboxutils-filelist.json";

/// Where the running kernel module reports its own version.
pub const LOADED_VERSION_PATH: &str = "/proc/driver/nvidia/version";

pub fn loaded_driver_version(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // Deliberately not anchored to "NVRM version:" -- the line has been reworded across releases, while the dotted version on it has not.
    text.lines().find_map(version_in)
}

/// The driver version the resolved files belong to, taken from the library names themselves (`libcuda.so.595.91.07`) rather than from the manifest, which does not state one.
pub fn staged_driver_version(found: &[Resolved]) -> Option<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in found {
        if let Some(v) = r
            .host_path
            .file_name()
            .and_then(|n| version_in(&n.to_string_lossy()))
        {
            *counts.entry(v).or_default() += 1;
        }
    }
    // Ties broken by the higher version, so the answer is stable rather than whichever the map happened to yield first.
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
        .map(|(v, _)| v)
}

/// The first `N.N.N` (or longer) dotted number in `s`, where every component is digits.
fn version_in(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dots = 0;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            if bytes[i] == b'.' {
                // "595.99.02." or "1..2" is not a version; stop before the dot.
                if i + 1 >= bytes.len() || !bytes[i + 1].is_ascii_digit() {
                    break;
                }
                dots += 1;
            }
            i += 1;
        }
        if dots >= 2 {
            return Some(s[start..i].to_string());
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    None
}

/// Directories a manifest entry's bare name is resolved against, in order.
pub const DEFAULT_SEARCH_PATHS: &[&str] = &[
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/bin",
    "/usr/share/vulkan/icd.d",
    "/usr/share/vulkan/implicit_layer.d",
    "/usr/share/vulkansc/icd.d",
    "/usr/share/glvnd/egl_vendor.d",
    "/usr/share/nvidia",
    "/usr/lib/nvidia",
    "/usr/lib/xorg/modules/drivers",
    "/usr/lib/xorg/modules/extensions",
];

/// What a file is, as the driver's manifest classifies it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    Lib,
    Binary,
    Json,
    Firmware,
    Symlink,
    Modprobe,
    Other(String),
}

impl FileKind {
    fn parse(s: &str) -> Self {
        match s {
            "LIB" => Self::Lib,
            "BINARY" => Self::Binary,
            "JSON" | "ICD" => Self::Json,
            "FIRMWARE" => Self::Firmware,
            "SYMLINK" => Self::Symlink,
            "MODPROBE" => Self::Modprobe,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lib => write!(f, "lib"),
            Self::Binary => write!(f, "binary"),
            Self::Json => write!(f, "json"),
            Self::Firmware => write!(f, "firmware"),
            Self::Symlink => write!(f, "symlink"),
            Self::Modprobe => write!(f, "modprobe"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

/// A coarse capability, in the sense container runtimes use the word: a name for a set of the driver's own finer-grained categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// `nvidia-smi` and NVML -- the smallest useful set, and enough to tell whether forwarding works at all.
    Utility,
    /// CUDA, OpenCL and the PTX JIT.
    Compute,
    /// Vulkan, EGL and GL, including the headless EGL a guest compositor uses.
    Graphics,
    /// NVENC and NVDEC.
    Video,
}

impl Capability {
    /// The driver's category names this capability covers.
    fn categories(self) -> &'static [&'static str] {
        match self {
            Self::Utility => &["nvml", "utils"],
            Self::Compute => &["cuda", "opencl", "optix"],
            Self::Graphics => &[
                "vulkan",
                "egl",
                "egl_headless",
                "egl_wayland",
                "egl_gbm",
                "egl_x11",
                "glx",
                "gbm",
                "glvnd",
            ],
            Self::Video => &["video"],
        }
    }

    /// What a headless render-and-encode guest needs: everything but the host-only categories.
    pub fn headless_pipeline() -> &'static [Capability] {
        &[
            Capability::Utility,
            Capability::Compute,
            Capability::Graphics,
            Capability::Video,
        ]
    }
}

/// One file the driver declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// As the manifest gives it: usually a bare filename, occasionally with a leading component such as `firmware/`.
    pub name: String,
    pub kind: FileKind,
    pub categories: BTreeSet<String>,
}

impl Entry {
    /// Whether any of `caps` covers this entry.
    pub fn wanted_by(&self, caps: &[Capability]) -> bool {
        caps.iter()
            .flat_map(|c| c.categories())
            .any(|c| self.categories.contains(*c))
    }

    /// Whether this entry must never be carried into a guest, whatever capability asked for it.
    pub fn is_host_only(&self) -> bool {
        matches!(self.kind, FileKind::Firmware | FileKind::Modprobe)
            || self.categories.contains("firmware")
            || self.categories.contains("modprobe")
    }
}

/// A resolved entry: a manifest line plus where it actually is on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub entry: Entry,
    pub host_path: PathBuf,
    /// Where it should land in the guest, relative to the share root.
    pub guest_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum UserspaceError {
    #[error("no driver manifest at {0} -- this host's driver predates one, or is not installed")]
    NoManifest(PathBuf),
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not the array of objects a driver manifest is: {detail}")]
    Malformed { path: PathBuf, detail: String },
}

/// Parse a driver manifest.
pub fn parse_manifest(src: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;

    skip_ws(b, &mut i);
    if b.get(i) != Some(&b'[') {
        return Err("expected a top-level array".into());
    }
    i += 1;

    loop {
        skip_ws(b, &mut i);
        match b.get(i) {
            Some(b']') => break,
            Some(b',') => {
                i += 1;
                continue;
            }
            Some(b'{') => {}
            Some(c) => return Err(format!("expected an object, found {:?}", *c as char)),
            None => return Err("file ended inside the array".into()),
        }
        i += 1;

        let mut name = None;
        let mut kind = None;
        let mut categories = BTreeSet::new();

        loop {
            skip_ws(b, &mut i);
            match b.get(i) {
                Some(b'}') => {
                    i += 1;
                    break;
                }
                Some(b',') => {
                    i += 1;
                    continue;
                }
                Some(b'"') => {}
                _ => return Err("expected a key".into()),
            }
            let key = read_string(b, &mut i)?;
            skip_ws(b, &mut i);
            if b.get(i) != Some(&b':') {
                return Err(format!("no ':' after key {key:?}"));
            }
            i += 1;
            skip_ws(b, &mut i);

            match b.get(i) {
                Some(b'"') => {
                    let v = read_string(b, &mut i)?;
                    match key.as_str() {
                        "name" => name = Some(v),
                        "type" => kind = Some(FileKind::parse(&v)),
                        _ => {}
                    }
                }
                Some(b'[') => {
                    i += 1;
                    loop {
                        skip_ws(b, &mut i);
                        match b.get(i) {
                            Some(b']') => {
                                i += 1;
                                break;
                            }
                            Some(b',') => {
                                i += 1;
                            }
                            Some(b'"') => {
                                let v = read_string(b, &mut i)?;
                                if key == "category" {
                                    categories.insert(v);
                                }
                            }
                            _ => return Err("expected a string in an array".into()),
                        }
                    }
                }
                // A value this parser does not model -- a number, bool or null.
                _ => skip_scalar(b, &mut i),
            }
        }

        let name = name.ok_or("an entry has no \"name\"")?;
        entries.push(Entry {
            name,
            kind: kind.unwrap_or(FileKind::Other(String::new())),
            categories,
        });
    }

    Ok(entries)
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while matches!(b.get(*i), Some(c) if c.is_ascii_whitespace()) {
        *i += 1;
    }
}

fn skip_scalar(b: &[u8], i: &mut usize) {
    while !matches!(b.get(*i), None | Some(b',') | Some(b'}') | Some(b']')) {
        *i += 1;
    }
}

fn read_string(b: &[u8], i: &mut usize) -> Result<String, String> {
    if b.get(*i) != Some(&b'"') {
        return Err("expected a string".into());
    }
    *i += 1;
    let mut out = String::new();
    loop {
        match b.get(*i) {
            None => return Err("file ended inside a string".into()),
            Some(b'"') => {
                *i += 1;
                return Ok(out);
            }
            Some(b'\\') => {
                *i += 1;
                match b.get(*i) {
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(c) => out.push(*c as char),
                    None => return Err("file ended inside an escape".into()),
                }
                *i += 1;
            }
            Some(c) => {
                out.push(*c as char);
                *i += 1;
            }
        }
    }
}

/// Rewrite a manifest's entries to name a different driver build.
pub fn retarget(entries: &[Entry], from: &str, to: &str) -> Vec<Entry> {
    entries
        .iter()
        .map(|e| Entry {
            name: e.name.replace(from, to),
            kind: e.kind.clone(),
            categories: e.categories.clone(),
        })
        .collect()
}

/// Read the manifest this host's driver installed.
pub fn load_manifest(path: &Path) -> Result<Vec<Entry>, UserspaceError> {
    if !path.exists() {
        return Err(UserspaceError::NoManifest(path.to_path_buf()));
    }
    let src = std::fs::read_to_string(path).map_err(|source| UserspaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_manifest(&src).map_err(|detail| UserspaceError::Malformed {
        path: path.to_path_buf(),
        detail,
    })
}

/// Where a file of this kind belongs in the guest, relative to the share root.
fn guest_dir(entry: &Entry, host_path: &Path) -> PathBuf {
    match entry.kind {
        FileKind::Binary | FileKind::Modprobe => PathBuf::from("bin"),
        FileKind::Json => {
            // Keep a JSON file under the same tail as the host has it, so the loader finds it where it expects: icd.d, implicit_layer.d and egl_vendor.d are not interchangeable, and the loader scans each by name.
            let parent = host_path.parent().unwrap_or(Path::new(""));
            let mut comps = parent.components();
            if comps.any(|c| c.as_os_str() == "share") {
                Path::new("share").join(comps.as_path())
            } else {
                match parent.file_name() {
                    Some(tail) => Path::new("share").join(tail),
                    None => PathBuf::from("share"),
                }
            }
        }
        _ => PathBuf::from("lib"),
    }
}

/// Find each wanted entry on this host.
pub fn resolve(
    entries: &[Entry],
    caps: &[Capability],
    search_paths: &[impl AsRef<Path>],
) -> (Vec<Resolved>, Vec<Entry>) {
    let mut found: Vec<Resolved> = Vec::new();
    let mut missing: Vec<Entry> = Vec::new();
    let mut staged = BTreeSet::new();
    let mut absent = BTreeSet::new();

    for entry in entries {
        if entry.is_host_only() || !entry.wanted_by(caps) {
            continue;
        }
        // The manifest may carry a leading directory component; only the file name is searched for, and the component is not reused as a path.
        let base = Path::new(&entry.name)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());

        match search_paths
            .iter()
            .map(|d| d.as_ref().join(&base))
            .find(|p| p.exists())
        {
            Some(host_path) => {
                let guest_path = guest_dir(entry, &host_path).join(&base);
                // Merge the categories of a repeated entry into the one that is kept, so the record says every capability the file serves.
                if let Some(prev) = found.iter_mut().find(|r| r.guest_path == guest_path) {
                    prev.entry
                        .categories
                        .extend(entry.categories.iter().cloned());
                    continue;
                }
                if staged.insert(guest_path.clone()) {
                    found.push(Resolved {
                        entry: entry.clone(),
                        host_path,
                        guest_path,
                    });
                }
            }
            None => {
                if absent.insert(entry.name.clone()) {
                    missing.push(entry.clone());
                }
            }
        }
    }

    (found, missing)
}

/// The `SONAME` a shared library records for itself, if it has one.
pub fn soname(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: i64 = 0;
    const DT_STRTAB: i64 = 5;
    const DT_SONAME: i64 = 14;

    let mut f = std::fs::File::open(path).ok()?;

    let mut ehdr = [0u8; 64];
    f.read_exact(&mut ehdr).ok()?;
    // Only ELF64 little-endian is in scope; this project is x86_64 host and guest, and a wrong-width parse would read garbage rather than fail.
    if &ehdr[0..4] != b"\x7fELF" || ehdr[4] != 2 || ehdr[5] != 1 {
        return None;
    }
    // Infallible by construction: every caller below indexes into a buffer it has already read to an exact, known size.
    let u16at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let u64at = |b: &[u8], o: usize| {
        u64::from_le_bytes([
            b[o],
            b[o + 1],
            b[o + 2],
            b[o + 3],
            b[o + 4],
            b[o + 5],
            b[o + 6],
            b[o + 7],
        ])
    };

    let phoff = u64at(&ehdr, 0x20);
    let phentsize = u16at(&ehdr, 0x36) as usize;
    let phnum = u16at(&ehdr, 0x38) as usize;
    if phentsize < 56 || phnum == 0 {
        return None;
    }

    let mut phdrs = vec![0u8; phentsize * phnum];
    f.seek(SeekFrom::Start(phoff)).ok()?;
    f.read_exact(&mut phdrs).ok()?;

    // DT_STRTAB is a virtual address, so it has to be translated back to a file offset through the PT_LOAD segments.
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum {
        let p = &phdrs[i * phentsize..][..56];
        let p_type = u32at(p, 0);
        let p_offset = u64at(p, 8);
        let p_vaddr = u64at(p, 16);
        let p_filesz = u64at(p, 32);
        match p_type {
            PT_LOAD => loads.push((p_vaddr, p_offset, p_filesz)),
            PT_DYNAMIC => dynamic = Some((p_offset, p_filesz)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic?;
    let vaddr_to_off = |v: u64| -> Option<u64> {
        loads
            .iter()
            .find(|(va, _, sz)| v >= *va && v < va + sz)
            .map(|(va, off, _)| off + (v - va))
    };

    let mut dynbuf = vec![0u8; dyn_size as usize];
    f.seek(SeekFrom::Start(dyn_off)).ok()?;
    f.read_exact(&mut dynbuf).ok()?;

    let mut soname_off = None;
    let mut strtab = None;
    for e in dynbuf.as_chunks::<16>().0 {
        let tag = i64::from_le_bytes(e[0..8].try_into().expect("16-byte chunk"));
        let val = u64::from_le_bytes(e[8..16].try_into().expect("16-byte chunk"));
        match tag {
            DT_NULL => break,
            DT_SONAME => soname_off = Some(val),
            DT_STRTAB => strtab = Some(val),
            _ => {}
        }
    }

    let strtab_file = vaddr_to_off(strtab?)?;
    let mut name = Vec::new();
    f.seek(SeekFrom::Start(strtab_file + soname_off?)).ok()?;
    // A SONAME is short; read a bounded window and stop at the NUL rather than trusting a length from the file.
    let mut window = [0u8; 256];
    let n = f.read(&mut window).ok()?;
    for &b in &window[..n] {
        if b == 0 {
            break;
        }
        name.push(b);
    }
    if name.is_empty() {
        return None;
    }
    String::from_utf8(name).ok()
}

#[cfg(test)]
#[path = "../tests/unit/userspace.rs"]
mod tests;
