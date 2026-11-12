//! GET_PROC_FILES and GET_SYS_FILES: the host trees a guest republishes, and the DRI section that follows them.

use super::*;

/// Which host tree a `GetProcFiles`/`GetSysFiles` request refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTree {
    /// `/proc/driver/nvidia`, which NVML reads before it will talk to a device.
    Proc,
    /// The sysfs attributes the userspace driver looks for on each card.
    Sys,
}

impl FileTree {
    fn name(self) -> &'static str {
        match self {
            Self::Proc => "GET_PROC_FILES",
            Self::Sys => "GET_SYS_FILES",
        }
    }

    fn root(self) -> &'static str {
        match self {
            Self::Proc => "/proc/driver/nvidia",
            // Paths in this stream are relative to /sys, because that is what the driver matches on: it looks for "bus/pci/devices/<addr>/ config" and ignores everything else.
            Self::Sys => "/sys",
        }
    }

    /// Read the tree, returning `(path relative to the root, contents)`.
    fn collect(self) -> Vec<(String, Vec<u8>)> {
        const MAX_FILE: u64 = 64 * 1024;
        match self {
            Self::Proc => {
                let mut out = Vec::new();
                let root = std::path::Path::new(self.root());
                collect_into(root, root, &mut out, MAX_FILE, 0);
                out.sort_by(|a, b| a.0.cmp(&b.0));
                // Paths in this stream are relative to /proc, not to the driver's own directory: the guest walks each component from the root of procfs to create the parents.
                for (path, _) in &mut out {
                    *path = format!("driver/nvidia/{path}");
                }
                out
            }
            // Not a walk. /sys is enormous, most of it is irrelevant, and some of it blocks on read.
            Self::Sys => crate::host::gpu_slots(std::path::Path::new(Self::Proc.root()))
                .iter()
                .filter_map(|slot| {
                    let end = slot
                        .pci_addr
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(slot.pci_addr.len());
                    let addr = String::from_utf8_lossy(&slot.pci_addr[..end]);
                    let rel = format!("bus/pci/devices/{addr}/config");
                    let abs = std::path::Path::new("/sys").join(&rel);
                    match std::fs::read(&abs) {
                        Ok(content) => Some((rel, content)),
                        Err(e) => {
                            log::warn!("sys: cannot read {}: {}", abs.display(), e);
                            None
                        }
                    }
                })
                .collect(),
        }
    }
}

/// Walk `dir`, appending every readable regular file under it.
fn collect_into(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, Vec<u8>)>,
    max_file: u64,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // symlink_metadata, not metadata: a symlink here leads out of the tree.
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if md.is_symlink() {
            continue;
        }
        if md.is_dir() {
            collect_into(root, &path, out, max_file, depth + 1);
            continue;
        }
        if !md.is_file() {
            continue;
        }
        // procfs reports zero length for files with real content, so size is only usable as an upper bound when it is non-zero.
        if md.len() > max_file {
            continue;
        }
        let Ok(content) = std::fs::read(&path) else {
            continue;
        };
        if content.len() as u64 > max_file {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            out.push((rel.to_string_lossy().into_owned(), content));
        }
    }
}

impl NvidiaBackend {
    // ------------------------------------------------------------------ GET_PROC_FILES / GET_SYS_FILES ------------------------------------------------------------------

    /// Collect a tree of small files and stream them to the guest.
    pub(super) fn handle_get_files(&mut self, tree: FileTree, resp_buf: &mut [u8]) -> usize {
        let files = tree.collect();
        log::info!("{}: {} file(s)", tree.name(), files.len());

        let mut off = 0usize;
        for (path, content) in &files {
            let need = size_of::<FileEntry>() + path.len() + content.len();
            // Leave room for the terminator, or a guest reads past the last entry into whatever the buffer held before.
            if off + need + size_of::<FileEntry>() > resp_buf.len() {
                log::warn!(
                    "{}: response buffer holds {} of {} files",
                    tree.name(),
                    files.iter().position(|(p, _)| p == path).unwrap_or(0),
                    files.len()
                );
                break;
            }
            off += write_struct(
                &mut resp_buf[off..],
                &FileEntry {
                    path_len: path.len() as u32,
                    content_len: content.len() as u32,
                },
            );
            resp_buf[off..off + path.len()].copy_from_slice(path.as_bytes());
            off += path.len();
            resp_buf[off..off + content.len()].copy_from_slice(content);
            off += content.len();
        }

        if off + size_of::<FileEntry>() <= resp_buf.len() {
            off += write_struct(&mut resp_buf[off..], &FileEntry::default());
        }

        // GET_SYS_FILES carries a second section the file stream does not announce: a u32 count of DRI devices, then that many records of {name_len, major, minor, slot_index, dev_info[9]} and the name.
        if tree == FileTree::Sys {
            off += self.write_dri_section(&mut resp_buf[off..]);
            off += self.write_alloc_size_section(&mut resp_buf[off..]);
            off += self.write_uvm_section(&mut resp_buf[off..]);
            off += self.write_osdesc_section(&mut resp_buf[off..]);
        }
        off
    }

    /// Magic word opening the allocation-size section.
    pub(super) const ALLOC_SIZE_MAGIC: u32 = 0x4e56_414c; // "NVAL"

    /// What RM sizes each allocation at, for the release the host is running.
    pub(super) fn write_alloc_size_section(&self, buf: &mut [u8]) -> usize {
        let Some(sel) = self.rmallow else {
            // Nothing learned about the host yet, so nothing to say.
            return 0;
        };
        let with_params: Vec<_> = sel.class.iter().filter(|c| c.params_size > 0).collect();
        if buf.len() < 8 + with_params.len() * 8 {
            log::warn!("no room for the allocation-size section; the guest will use its own table");
            return 0;
        }
        let mut off = 0;
        for v in [Self::ALLOC_SIZE_MAGIC, with_params.len() as u32] {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
            off += 4;
        }
        for c in &with_params {
            for v in [c.class_id, c.params_size] {
                buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
                off += 4;
            }
        }
        log::info!(
            "GET_SYS_FILES: RM's allocation sizes for {} class(es)",
            with_params.len()
        );
        off
    }

    /// Magic word opening the UVM section, and what each record means.
    pub(super) const UVM_CMD_MAGIC: u32 = 0x4e56_5556; // "NVUV"
    const UVM_FD_NONE: u32 = 0;
    const UVM_FD_CTL: u32 = 1;
    const UVM_FD_UVM: u32 = 2;
    const UVM_FD_FOREIGN: u32 = 3;

    /// The UVM commands the host release takes, their sizes, and where a descriptor sits in each.
    pub(super) fn write_uvm_section(&self, buf: &mut [u8]) -> usize {
        let Some(sel) = self.uvm else {
            // No release known, so nothing to say.
            return 0;
        };
        const REC: usize = 16;
        let mut rows = Vec::new();
        for c in sel.cmd {
            // One descriptor is all the record has room for.
            let (kind, at) = match c.fds {
                [] => (Self::UVM_FD_NONE, 0),
                [one] => (
                    match one.kind {
                        abi::uvm::Fd::Ctl => Self::UVM_FD_CTL,
                        abi::uvm::Fd::Uvm => Self::UVM_FD_UVM,
                        abi::uvm::Fd::Foreign => Self::UVM_FD_FOREIGN,
                    },
                    one.at as u32,
                ),
                more => {
                    log::warn!(
                        "UVM {:#x} carries {} descriptors and the guest is told of one at most;                          leaving it out, which refuses it",
                        c.num,
                        more.len()
                    );
                    continue;
                }
            };
            rows.push([c.num, c.params_size, kind, at]);
        }

        if buf.len() < 8 + rows.len() * REC {
            log::warn!("no room for the UVM section; the guest will serve no UVM call");
            return 0;
        }
        let mut off = 0;
        for v in [Self::UVM_CMD_MAGIC, rows.len() as u32] {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
            off += 4;
        }
        for r in &rows {
            for v in r {
                buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
                off += 4;
            }
        }
        log::info!(
            "GET_SYS_FILES: {} UVM command(s), {} carrying a descriptor",
            rows.len(),
            rows.iter().filter(|r| r[2] != Self::UVM_FD_NONE).count()
        );
        off
    }

    /// Magic word opening the OS-descriptor section.
    pub(super) const OSDESC_MAGIC: u32 = 0x4e564f44; // "NVOD"

    /// Where the host release keeps the CPU address on each route that names memory by one.
    pub(super) fn write_osdesc_section(&self, buf: &mut [u8]) -> usize {
        let Some(d) = self.osdesc else {
            return 0;
        };
        // magic, class, vid_heap_function, vid_heap_function_at, alloc_memory_class_at, virtual_address, then four words per route.
        let words: [u32; 22] = [
            Self::OSDESC_MAGIC,
            d.class,
            d.vid_heap_function,
            d.vid_heap_function_at as u32,
            d.alloc_memory_class_at as u32,
            d.virtual_address,
            // Where each route reports what it did: the status RM writes, and for the heap route the handle the registration comes back under.
            d.alloc_memory_status_at as u32,
            d.vid_heap_status_at as u32,
            d.vid_heap_hmemory_at as u32,
            0, // reserved, so the routes below stay on a round offset
            d.alloc.params_size as u32,
            d.alloc.address_at as u32,
            d.alloc.limit_at as u32,
            d.alloc.type_at as u32,
            d.alloc_memory.params_size as u32,
            d.alloc_memory.address_at as u32,
            d.alloc_memory.limit_at as u32,
            // `usize::MAX` means the route has no descriptor type; narrowed to u32 it is still a value no offset can be.
            d.alloc_memory.type_at as u32,
            d.vid_heap.params_size as u32,
            d.vid_heap.address_at as u32,
            d.vid_heap.limit_at as u32,
            d.vid_heap.type_at as u32,
        ];
        if buf.len() < words.len() * 4 {
            log::warn!("no room for the OS-descriptor section; the guest will register nothing");
            return 0;
        }
        let mut off = 0;
        for w in words {
            buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            off += 4;
        }
        log::info!(
            "GET_SYS_FILES: memory may be registered by address on 3 routes, class {:#06x}",
            d.class
        );
        off
    }

    /// The DRI section of a `GetSysFiles` response.
    pub(super) fn write_dri_section(&self, buf: &mut [u8]) -> usize {
        // Without graphics there is no render node to offer, and a guest told of none creates none, whatever it knows about capabilities.
        let devices = if self.caps.has(crate::caps::GRAPHICS) {
            self.dri_devices()
        } else {
            Vec::new()
        };
        // Kept: the guest numbers nodes by this list, so an open must be resolved against it and not against a fresh scan of sysfs, which came back empty under ten guests' load and refused the open.
        *self.dri_given.borrow_mut() = Some(devices.clone());
        log::info!("GET_SYS_FILES: {} DRI device(s)", devices.len());

        if buf.len() < 4 {
            return 0;
        }
        let mut off = 0;
        buf[off..off + 4].copy_from_slice(&(devices.len() as u32).to_le_bytes());
        off += 4;

        for d in &devices {
            // name_len, major, minor, slot_index, then the nine dev_info words.
            let need = 16 + 4 * NV_DEV_INFO_WORDS + d.name.len();
            if off + need > buf.len() {
                log::warn!("DRI section truncated at {}", d.name);
                break;
            }
            for v in [d.name.len() as u32, d.major, d.minor, d.slot_index]
                .into_iter()
                .chain(d.dev_info)
            {
                buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
                off += 4;
            }
            buf[off..off + d.name.len()].copy_from_slice(d.name.as_bytes());
            off += d.name.len();
        }
        off
    }

    /// The render nodes the host's GPUs own.
    pub(super) fn dri_devices(&self) -> Vec<DriDevice> {
        let mut out = Vec::new();
        for (index, slot) in crate::host::gpu_slots(std::path::Path::new(FileTree::Proc.root()))
            .iter()
            .enumerate()
        {
            let end = slot
                .pci_addr
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(slot.pci_addr.len());
            let addr = String::from_utf8_lossy(&slot.pci_addr[..end]).into_owned();
            let dir = format!("/sys/bus/pci/devices/{addr}/drm");

            let Ok(entries) = std::fs::read_dir(&dir) else {
                log::warn!("no DRI nodes under {dir}");
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .collect();
            names.sort();

            for name in names {
                // The kernel prints "major:minor" here.
                let Ok(text) = std::fs::read_to_string(format!("/sys/class/drm/{name}/dev")) else {
                    log::warn!("DRI node {name} has no dev file");
                    continue;
                };
                let text = text.trim();
                let Some((maj, min)) = text.split_once(':') else {
                    log::warn!("DRI node {name}: cannot read {text:?} as major:minor");
                    continue;
                };
                let (Ok(major), Ok(minor)) = (maj.parse::<u32>(), min.parse::<u32>()) else {
                    log::warn!("DRI node {name}: cannot read {text:?} as major:minor");
                    continue;
                };
                let dev_info = host_dev_info(&format!("/dev/dri/{name}")).unwrap_or_else(|| {
                    // Same shape the guest used to invent, so a refusal is no worse than the old behaviour -- but it is logged above.
                    let mut fallback = [0u32; NV_DEV_INFO_WORDS];
                    fallback[3] = 1; // supports_alloc
                    fallback[4] = 6; // generic_page_kind
                    fallback[5] = 2; // page_kind_generation
                    fallback[6] = 1; // sector_layout
                    fallback[7] = 1; // supports_sync_fd
                    fallback[8] = 1; // supports_semsurf
                    fallback
                });
                log::info!(
                    "DRI {name} at {major}:{minor} on {addr} (slot {index}, \
                     nvidia gpu_id {:#x}, page kind {}/{}, sector layout {})",
                    dev_info[0],
                    dev_info[4],
                    dev_info[5],
                    dev_info[6],
                );
                out.push(DriDevice {
                    name,
                    major,
                    minor,
                    slot_index: index as u32,
                    dev_info,
                });
            }
        }
        out
    }
}
