mod legion_event_retry;
mod legion_poll_interest;
mod legion_quiet_arm;
mod legion_ready_fences;
use legion_event_retry::PendingEvents;

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use device::caps::Caps;
use device::host;
use device::nvidia::NvidiaBackend;
use device::shm::{WindowPlacer, ZoneConfig};
use device::virtio::{NUM_QUEUES, QUEUE_SIZE, VIRTIO_ID_GPU_NV, VirtioGpuNvConfig};
use protocol::messages::{MsgHeader, MsgType};
use std::os::fd::{BorrowedFd, RawFd};
use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserShMemConfig,
    VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Backend, VhostUserFrontendReqHandler};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::{QueueOwnedT, QueueT};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend,
    GuestMemoryLoadGuard, GuestMemoryMmap, GuestMemoryRegion,
};
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

/// The driver calls `virtio_find_vqs(vdev, 2, ...)` and fails probe on the error from that call, so offering fewer is fatal before config is read.
const QUEUE_COUNT: usize = NUM_QUEUES;
/// Initial scratch size; ordinary requests do not allocate more.
const RESP_INITIAL: usize = 64 * 1024;
/// Bounds the guest's 1 MiB nested block, 2 MiB segments, and headers.
const RESP_MAX: usize = 4 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(version, about = "vhost-user backend for virtio-nvgpu")]
struct Args {
    /// Unix socket QEMU connects to.
    #[arg(long, default_value = "/tmp/nvgpu.sock")]
    socket: String,

    /// Where the host driver publishes itself.
    #[arg(long, default_value = host::PROC_NVIDIA)]
    proc_nvidia: PathBuf,

    /// What the guest is served: a comma list of graphics, compute, video and utility.
    #[arg(long, default_value_t = Caps::DEFAULT, value_parser = Caps::parse)]
    caps: Caps,

    /// Video memory the guest may hold, in MiB.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    vram_limit_mib: Option<u64>,

    /// Uncached device window address space, in MiB (not resident RAM).
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u64).range(1..=16384))]
    shm_uncached_mib: u64,

    /// Write-combine device window address space, in MiB.
    #[arg(long, default_value_t = 768, value_parser = clap::value_parser!(u64).range(1..=16384))]
    shm_write_combine_mib: u64,

    /// Write-back device window address space, in MiB.
    #[arg(long, default_value_t = 224, value_parser = clap::value_parser!(u64).range(1..=16384))]
    shm_write_back_mib: u64,

    /// Experimental host Wayland presentation, WIDTHxHEIGHT.
    #[arg(long)]
    legion_display: Option<String>,
}

/// Places device memory through the vhost-user backend request channel.
struct VhostGuestRam(GuestMemoryAtomic<GuestMemoryMmap>);

impl device::guestmem::GuestRam for VhostGuestRam {
    fn backing(&self, gpa: u64) -> Option<device::guestmem::Backing> {
        let mem = self.0.memory();
        let region = mem.find_region(GuestAddress(gpa))?;
        // A region the frontend sent without a descriptor cannot be mapped from, only read through.
        let file = region.file_offset()?;
        let within = gpa.checked_sub(region.start_addr().raw_value())?;
        Some(device::guestmem::Backing {
            fd: file.file().try_clone().ok()?.into(),
            offset: file.start().checked_add(within)?,
            len: region.len().checked_sub(within)?,
        })
    }
}

struct VhostWindow(Backend);

impl WindowPlacer for VhostWindow {
    fn place(
        &self,
        shm_offset: u64,
        len: u64,
        fd: RawFd,
        fd_offset: u64,
        writable: bool,
    ) -> device::error::Result<()> {
        let len = device::legion_window_pages::transport_extent(len)?;
        let req = VhostUserMMap {
            shmid: NV_SHM_ID,
            padding: [0; 7],
            fd_offset,
            shm_offset,
            len,
            flags: if writable {
                VhostUserMMapFlags::WRITABLE.bits()
            } else {
                0
            },
        };
        // SAFETY: the descriptor is owned by the handle table for the whole of this call, and is only borrowed to be sent.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn withdraw(&self, shm_offset: u64, len: u64) -> device::error::Result<()> {
        let len = device::legion_window_pages::transport_extent(len)?;
        let req = VhostUserMMap {
            shmid: NV_SHM_ID,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset,
            len,
            flags: 0,
        };
        self.0
            .shmem_unmap(&req)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn place_pool(&self, offset: u64, len: u64, fd: RawFd, addr: u64) -> device::error::Result<()> {
        // On the aperture, fd_offset is the pool's host address as well as its offset in the file: UVM takes the mapping nowhere else.
        let req = VhostUserMMap {
            shmid: NV_SHM_ID_APERTURE,
            padding: [0; 7],
            fd_offset: addr,
            shm_offset: offset,
            len,
            flags: VhostUserMMapFlags::WRITABLE.bits(),
        };
        // SAFETY: as in `place`.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn withdraw_pool(&self, offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID_APERTURE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: offset,
            len,
            flags: 0,
        };
        self.0
            .shmem_unmap(&req)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }
}

/// What the event thread is told to start and stop watching.
enum Watch {
    Display(OwnedFd),
    Fence(u64, OwnedFd),
    RemoveFence(u64),
    Add(u32, OwnedFd),
    Subscribe(u32),
    Remove(u32),
}

/// One `EventReady` message: a bare header naming the descriptor.
fn event_ready_bytes(handle: u32) -> [u8; size_of::<MsgHeader>()] {
    const {
        assert!(size_of::<MsgHeader>() == 16);
    }
    let mut out = [0; size_of::<MsgHeader>()];
    out[..4].copy_from_slice(&(MsgType::EventReady as u32).to_le_bytes());
    out[4..8].copy_from_slice(&handle.to_le_bytes());
    out
}

/// Put one message on the event queue, into a buffer the guest posted there.
fn push_event(vring: &VringRwLock, mem: &GuestMemoryAtomic<GuestMemoryMmap>, handle: u32) -> bool {
    push_event_data(vring, mem, &event_ready_bytes(handle), true)
}

// virtio-queue implements EVENT_IDX with the publication barrier and wrapped indices.
fn notify_used(
    vring: &VringRwLock,
    mem: &GuestMemoryMmap,
    completed: bool,
    permit_irq: bool,
) -> std::io::Result<bool> {
    if !completed {
        return Ok(false);
    }
    let wanted = {
        let mut state = vring.get_mut();
        if state.get_queue().event_idx_enabled() {
            state
                .needs_notification()
                .map_err(|e| std::io::Error::other(format!("guest event index: {e}")))?
        } else {
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            let flags = mem
                .read_obj::<u16>(GuestAddress(state.get_queue().avail_ring()))
                .map_err(|e| std::io::Error::other(format!("guest avail flags: {e}")))?;
            u16::from_le(flags) & 1 == 0 // VRING_AVAIL_F_NO_INTERRUPT
        }
    };
    // Consume EVENT_IDX accounting even when an explicit active-poll transport suppresses interrupts.
    let signal = wanted && permit_irq;
    if signal {
        vring.signal_used_queue()?;
    }
    Ok(signal)
}

fn push_event_data(
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    bytes: &[u8],
    notify: bool,
) -> bool {
    let guard = mem.memory();
    let chain = loop {
        let mut vr = vring.get_mut();
        let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
            return false;
        };
        if let Some(chain) = avail.next() {
            break chain;
        }
        drop(avail);
        // Re-arm before parking.
        match vr.enable_notification() {
            Ok(true) => continue,
            _ => return false,
        }
    };
    let head = chain.head_index();

    let mut written = 0usize;
    for desc in chain {
        if desc.is_write_only() {
            let n = if desc.len() as usize >= bytes.len() {
                bytes.len()
            } else {
                0
            };
            if guard.write_slice(&bytes[..n], desc.addr()).is_ok() {
                written = n;
            }
            break;
        }
    }

    if vring.add_used(head, written as u32).is_err() {
        return false;
    }
    if let Err(error) = notify_used(vring, &guard, true, notify) {
        // The record is already committed.
        log::error!("event notification: {error}");
        if notify {
            let _ = vring.signal_used_queue();
        }
    }
    written == bytes.len()
}

/// Watch the host's descriptors and tell the guest when one has something to say.
const DISPLAY_EVENT_KEY: u64 = u64::MAX;
// Wire file handles use 32 bits; optional fence keys use bit 63.
const PUMP_WAKE_KEY: u64 = 1_u64 << 32;

fn display_events(
    nvidia: &Arc<Mutex<NvidiaBackend>>,
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
) {
    let Ok(mut backend) = nvidia.lock() else {
        log::error!("display event mutex poisoned");
        return;
    };
    let event = backend.prepare_legion_display_event();
    drop(backend); // Never hold the backend lock while taking the event virtqueue.
    if let Some((owner, status, payload, notify)) = event {
        let mut packet = [0; 280];
        packet[..16].copy_from_slice(&event_ready_bytes(owner));
        packet[8..12].copy_from_slice(&status.to_le_bytes());
        packet[12..16].copy_from_slice(&0x4c47_0001_u32.to_le_bytes());
        packet[16..].copy_from_slice(&payload);
        if push_event_data(vring, mem, &packet, notify)
            && let Ok(mut backend) = nvidia.lock()
        {
            backend.finish_legion_display_event(owner);
        }
    }
}

const FENCE_EVENT_KEY: u64 = 1_u64 << 63;

// True means the one-shot watch can retire.
fn fence_ready(
    nvidia: &Arc<Mutex<NvidiaBackend>>,
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    key: u64,
) -> bool {
    let token = key & !FENCE_EVENT_KEY;
    let packet = nvidia
        .lock()
        .expect("backend mutex")
        .begin_legion_fence_event(token);
    if let Some(packet) = packet {
        let accepted = push_event_data(vring, mem, &packet, true);
        // CLOSE may run between queue publication and this receipt.
        nvidia
            .lock()
            .expect("backend mutex")
            .finish_legion_fence_event(token, accepted);
        return accepted;
    }
    nvidia
        .lock()
        .expect("backend mutex")
        .legion_fence_watch_finished(token)
}

include!("legion_poll_arm_pump.rs");

/// The queue the host posts events on.
const EVENT_QUEUE: usize = 1;

/// The shared-memory id the guest driver looks the window up by, which must match the capability the VMM publishes.
const NV_SHM_ID: u8 = 1;
/// The UVM aperture, where semaphore pools are placed.
const NV_SHM_ID_APERTURE: u8 = 2;

struct NvGpuBackend {
    nvidia: Arc<Mutex<NvidiaBackend>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    quiet_arms_offered: bool,
    quiet_arms: bool,
    control_batch: legion_quiet_arm::ControlBatch,
    config: VirtioGpuNvConfig,
    /// Started on the first message, because the event queue and guest memory are not known before then.
    watches: Option<Sender<Watch>>,
    poll_subscriptions: std::collections::HashSet<u32>,
    event_wake: Arc<EventFd>,
    display_watch: bool,
    /// The request and response of the chain being served, kept across chains.
    req: Vec<u8>,
    resp: Vec<u8>,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    fn new(
        proc_nvidia: &Path,
        caps: Caps,
        vram_limit_mib: Option<u64>,
        zones: ZoneConfig,
    ) -> anyhow::Result<Self> {
        let version = host::driver_version(proc_nvidia).ok_or_else(|| {
            anyhow::anyhow!(
                "no NVIDIA driver version at {} -- is the kernel module loaded?",
                proc_nvidia.display()
            )
        })?;
        let gpus = host::gpu_slots(proc_nvidia);
        anyhow::ensure!(
            !gpus.is_empty(),
            "driver {version} is loaded but owns no GPUs; the guest driver rejects an empty table"
        );
        log::info!("host driver {version}, {} GPU(s)", gpus.len());

        log::info!(
            "device window bytes: UC={}, WC={}, WB={}",
            zones.uc_size,
            zones.wc_size,
            zones.wb_size
        );
        let mut nvidia = NvidiaBackend::new(zones);
        let release = abi::version::DriverVersion::parse(&version)
            .ok_or_else(|| anyhow::anyhow!("host driver version {version:?} does not parse"))?;
        nvidia
            .set_host_driver_version(release)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
        nvidia.set_caps(caps);
        nvidia
            .set_vram_limit_mib(vram_limit_mib)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;

        let nvidia_vram_mib = nvidia.vram_limit_mib();
        Ok(Self {
            nvidia: Arc::new(Mutex::new(nvidia)),
            mem: None,
            event_idx: false,
            quiet_arms_offered: std::env::var("LEGION_QUIET_POLL_ARM").as_deref() == Ok("1"),
            quiet_arms: false,
            control_batch: legion_quiet_arm::ControlBatch::default(),

            config: VirtioGpuNvConfig::new(&version, &gpus, caps, nvidia_vram_mib),
            watches: None,
            poll_subscriptions: std::collections::HashSet::new(),
            event_wake: Arc::new(EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK)?),
            display_watch: false,
            req: Vec::new(),
            resp: vec![0u8; RESP_INITIAL],
        })
    }

    /// Keep the event thread's poll set in step with the descriptors the backend has open, starting the thread on first use.
    fn sync_watches(&mut self, vrings: &[VringRwLock]) {
        let (added, removed) = self
            .nvidia
            .lock()
            .expect("backend mutex")
            .take_watch_updates();
        let (fence_added, fence_removed) = self
            .nvidia
            .lock()
            .expect("backend mutex")
            .take_legion_fence_watches();
        let display_fd = if self.display_watch {
            None
        } else {
            self.nvidia
                .lock()
                .expect("backend mutex")
                .legion_display_event_fd()
        };
        if added.is_empty()
            && removed.is_empty()
            && self.poll_subscriptions.is_empty()
            && fence_added.is_empty()
            && fence_removed.is_empty()
            && display_fd.is_none()
            && self.watches.is_some()
        {
            return;
        }

        if self.watches.is_none() {
            let (Some(mem), Some(vring)) = (self.mem.clone(), vrings.get(1).cloned()) else {
                return;
            };
            let (tx, rx) = channel();
            let nvidia = self.nvidia.clone();
            let wake = self.event_wake.clone();
            std::thread::Builder::new()
                .name("nvgpu-events".into())
                .spawn(move || event_pump(rx, vring, mem, nvidia, wake))
                .map(|_| self.watches = Some(tx))
                .unwrap_or_else(|e| log::error!("event pump would not start: {e}"));
        }
        let Some(tx) = self.watches.as_ref() else {
            return;
        };

        if let Some(fd) = display_fd
            && tx.send(Watch::Display(fd)).is_ok()
        {
            self.display_watch = true;
        }
        for (handle, fd) in added {
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                log::warn!(
                    "watch on handle {handle}: dup: {}",
                    std::io::Error::last_os_error()
                );
                continue;
            }
            let _ = tx.send(Watch::Add(handle, unsafe { OwnedFd::from_raw_fd(dup) }));
        }
        for handle in removed {
            self.poll_subscriptions.remove(&handle);
            let _ = tx.send(Watch::Remove(handle));
        }
        for handle in self.poll_subscriptions.drain() {
            let _ = tx.send(Watch::Subscribe(handle));
        }
        for (token, fd) in fence_added {
            let _ = tx.send(Watch::Fence(token, fd));
        }
        for token in fence_removed {
            let _ = tx.send(Watch::RemoveFence(token));
        }
        // Publish channel updates before waking the reader.
        if let Err(error) = self.event_wake.write(1)
            && error.kind() != std::io::ErrorKind::WouldBlock
        {
            log::error!("event pump watch wake: {error}");
        }
    }

    /// Drain one virtqueue, dispatching every chain.
    fn process(
        &mut self,
        vring: &VringRwLock,
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> std::io::Result<bool> {
        let mut used = false;
        loop {
            let mut guard = vring.get_mut();
            let Ok(mut avail) = guard.get_queue_mut().iter(mem.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else { break };
            drop(guard);

            let head = chain.head_index();
            let mut resp_desc = None;
            let mut req_len = 0;

            for desc in chain.clone() {
                if desc.is_write_only() {
                    resp_desc = Some(desc);
                } else {
                    let at = req_len;
                    req_len += desc.len() as usize;
                    if req_len > self.req.len() {
                        self.req.resize(req_len, 0);
                    }
                    mem.read_slice(&mut self.req[at..req_len], desc.addr())
                        .map_err(|e| {
                            std::io::Error::other(format!("read request descriptor: {e}"))
                        })?;
                }
            }

            let mut acknowledged_quiet = false;
            let written = match resp_desc {
                Some(d) => {
                    let cap = std::cmp::min(d.len() as usize, RESP_MAX);
                    if self.resp.len() < cap {
                        self.resp.resize(cap, 0);
                    }
                    let resp = &mut self.resp[..cap];
                    let quiet = legion_quiet_arm::request(&self.req[..req_len], self.quiet_arms);
                    let n = if let Some(request) =
                        quiet.or_else(|| legion_poll_interest::arm_request(&self.req[..req_len]))
                    {
                        if resp.len() < 16 {
                            0
                        } else {
                            let (handle, status) = match request {
                                Ok(handle)
                                    if self
                                        .nvidia
                                        .lock()
                                        .expect("backend mutex")
                                        .poll_handle_is_live(handle) =>
                                {
                                    self.poll_subscriptions.insert(handle);
                                    (handle, 0)
                                }
                                Ok(handle) => (handle, -libc::EBADF),
                                Err(()) => (0, -libc::EINVAL),
                            };
                            acknowledged_quiet = quiet.is_some() && status == 0;
                            let reply = if quiet.is_some() {
                                legion_quiet_arm::reply(handle, status)
                            } else {
                                legion_poll_interest::arm_reply(handle, status)
                            };
                            resp[..16].copy_from_slice(&reply);
                            16
                        }
                    } else {
                        self.nvidia
                            .lock()
                            .expect("backend mutex")
                            .dispatch(&self.req[..req_len], resp)
                    };
                    if n > 0 {
                        mem.write_slice(&resp[..n], d.addr()).map_err(|e| {
                            std::io::Error::other(format!("write response descriptor: {e}"))
                        })?;
                    }
                    n
                }
                None => {
                    log::warn!("chain {head} has no writable descriptor; dropping");
                    0
                }
            };

            vring
                .add_used(head, written as u32)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            self.control_batch.completed(acknowledged_quiet);
            used = true;
        }
        Ok(used)
    }
}

impl VhostUserBackendMut for NvGpuBackend {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        QUEUE_COUNT
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE as usize
    }

    fn features(&self) -> u64 {
        (if self.quiet_arms_offered {
            legion_quiet_arm::FEATURE
        } else {
            0
        }) | (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn acked_features(&mut self, features: u64) {
        self.quiet_arms = self.quiet_arms_offered && features & legion_quiet_arm::FEATURE != 0;
        log::info!(
            "quiet poll-arm acknowledgements negotiated: {}",
            self.quiet_arms
        );
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            // The channel mapping requests travel up on, and the feature that gates the request itself.
            | VhostUserProtocolFeatures::BACKEND_REQ
            | VhostUserProtocolFeatures::SHMEM
            | VhostUserProtocolFeatures::REPLY_ACK
    }

    fn get_shmem_config(&self) -> std::io::Result<VhostUserShMemConfig> {
        let nvidia = self
            .nvidia
            .lock()
            .map_err(|_| std::io::Error::other("NVIDIA backend lock is poisoned"))?;
        // IDs must match VhostWindow and the guest's virtio_get_shm_region. nregions counts present regions; zero at index 0 is an absent ID.
        Ok(VhostUserShMemConfig::new(
            2,
            &[0, nvidia.shm_total_size(), device::nvidia::APERTURE_LEN],
        ))
    }

    fn set_backend_req_fd(&mut self, backend: Backend) {
        log::info!("window: request channel open; device memory is now mappable");
        self.nvidia
            .lock()
            .unwrap()
            .set_window(Box::new(VhostWindow(backend)));
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        self.config.read(offset, size)
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        // Also the backend's, so memory a guest registers by CPU address can be found.
        self.nvidia
            .lock()
            .unwrap()
            .set_guest_ram(Box::new(VhostGuestRam(mem.clone())));
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[VringRwLock],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        if device_event as usize >= QUEUE_COUNT {
            return Err(std::io::Error::other(format!(
                "event for unknown queue {device_event}"
            )));
        }
        // The event queue carries buffers the guest posted for *us* to fill, not requests.
        if device_event as usize == EVENT_QUEUE {
            // The pump owns buffer consumption.
            if let Err(error) = self.event_wake.write(1)
                && error.kind() != std::io::ErrorKind::WouldBlock
            {
                return Err(error);
            }
            return Ok(());
        }

        let mem = self
            .mem
            .as_ref()
            .ok_or_else(|| std::io::Error::other("guest memory not set"))?
            .memory();

        let vring = &vrings[device_event as usize];
        self.control_batch = legion_quiet_arm::ControlBatch::default();
        let mut completed = false;
        if self.event_idx {
            // With EVENT_IDX the guest suppresses notifications, so re-arm and drain again rather than waiting for a kick that will not come.
            loop {
                vring.disable_notification().ok();
                completed |= self.process(vring, &mem)?;
                if !vring.enable_notification().unwrap_or(false) {
                    break;
                }
            }
        } else {
            completed |= self.process(vring, &mem)?;
        }
        // After serving, not before: a message that opened a descriptor has to have been served for the backend to know about it.
        self.sync_watches(vrings);
        notify_used(vring, &mem, completed, self.control_batch.interrupt)?;
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    // Before any device is opened: the host driver judges every guest call by this process's credentials (device::posture).
    device::posture::enforce()?;

    // The sandbox, before anything else: Landlock and seccomp cover this thread and every thread started after them, and `VhostUserDaemon::new` starts one.
    let socket_dir = Path::new(&args.socket)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let report = device::sandbox::enter(&device::sandbox::Paths {
        devices: device::sandbox::gpu_nodes(),
        read_only: vec![
            args.proc_nvidia.clone(),
            PathBuf::from("/sys/class/drm"),
            PathBuf::from("/sys/bus/pci/devices"),
            PathBuf::from("/sys/devices"),
        ],
        sockets: vec![socket_dir],
    })?;
    log::info!(
        "sandbox: landlock ABI {}, {} seccomp instructions; no path outside the GPU nodes, the \
         driver's own trees and the socket's directory, no executable mapping, no process, no \
         socket but AF_UNIX",
        report.landlock_abi,
        report.seccomp_rules
    );

    log::info!(
        "virtio-nvgpu vhost-user backend: device id {VIRTIO_ID_GPU_NV}, socket {}, caps {}, {}",
        args.socket,
        args.caps,
        match args.vram_limit_mib {
            Some(m) => format!("video memory limited to {m} MiB"),
            None => "no video memory limit".to_string(),
        }
    );

    let backend = Arc::new(RwLock::new(NvGpuBackend::new(
        &args.proc_nvidia,
        args.caps,
        args.vram_limit_mib,
        ZoneConfig {
            uc_size: args.shm_uncached_mib * 1024 * 1024,
            wc_size: args.shm_write_combine_mib * 1024 * 1024,
            wb_size: args.shm_write_back_mib * 1024 * 1024,
        },
    )?));
    if let Some(size) = &args.legion_display {
        let (width, height) = size
            .split_once('x')
            .ok_or_else(|| anyhow::anyhow!("display size must be WIDTHxHEIGHT"))?;
        backend
            .write()
            .map_err(|_| anyhow::anyhow!("backend lock"))?
            .nvidia
            .lock()
            .map_err(|_| anyhow::anyhow!("NVIDIA lock"))?
            .enable_legion_display(width.parse()?, height.parse()?)?;
    }
    // vhost_user_backend::Error does not implement std::error::Error, so it cannot ride `?` on its own.
    let mut daemon = VhostUserDaemon::new(
        "virtio-nvgpu".to_string(),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("create daemon: {e:?}"))?;

    let _ = std::fs::remove_file(&args.socket);
    daemon
        .serve(&args.socket)
        .map_err(|e| anyhow::anyhow!("serve {}: {e:?}", args.socket))?;

    backend
        .write()
        .expect("backend lock")
        .nvidia
        .lock()
        .expect("nvidia lock")
        .teardown();
    log::info!("backend exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn shared_memory_configuration_matches_guest_region_ids() {
        let nvidia = NvidiaBackend::with_default_zones();
        let window_size = nvidia.shm_total_size();
        let backend = NvGpuBackend {
            nvidia: Arc::new(Mutex::new(nvidia)),
            mem: None,
            event_idx: false,
            quiet_arms_offered: false,
            quiet_arms: false,
            control_batch: legion_quiet_arm::ControlBatch::default(),
            config: VirtioGpuNvConfig::default(),
            watches: None,
            poll_subscriptions: std::collections::HashSet::new(),
            event_wake: Arc::new(EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK).expect("test wake fd")),
            display_watch: false,
            req: Vec::new(),
            resp: Vec::new(),
        };
        let layout = backend.get_shmem_config().expect("shared memory layout");
        assert_eq!(layout.nregions, 2);
        assert_eq!(layout.memory_sizes[0], 0);
        assert_eq!(layout.memory_sizes[usize::from(NV_SHM_ID)], window_size);
        assert_eq!(
            layout.memory_sizes[usize::from(NV_SHM_ID_APERTURE)],
            1 << 30
        );
        assert!(layout.memory_sizes[3..].iter().all(|size| *size == 0));
    }

    #[test]
    fn empty_kicks_and_suppressed_used_entries_do_not_request_interrupts() {
        let (memory, ring) = queue();
        let guard = memory.memory();
        assert!(!notify_used(&ring, &guard, false, true).unwrap());
        // EVENT_IDX used_event is the final u16 in the 8-entry avail ring.
        guard
            .write_obj(1_u16.to_le(), GuestAddress(0x2014))
            .unwrap();
        ring.add_used(0, 16).unwrap();
        assert!(!notify_used(&ring, &guard, true, true).unwrap());
        assert_eq!(used(&guard), 1);
        ring.add_used(1, 16).unwrap();
        assert!(notify_used(&ring, &guard, true, true).unwrap());
        assert!(!notify_used(&ring, &guard, false, true).unwrap());
    }

    #[test]
    fn legacy_no_interrupt_flag_preserves_completed_responses() {
        let (memory, ring) = queue();
        let guard = memory.memory();
        ring.set_queue_event_idx(false);
        guard
            .write_obj(1_u16.to_le(), GuestAddress(0x2000))
            .unwrap();
        ring.add_used(0, 16).unwrap();
        assert!(!notify_used(&ring, &guard, true, true).unwrap());
        assert_eq!(used(&guard), 1);
        guard
            .write_obj(0_u16.to_le(), GuestAddress(0x2000))
            .unwrap();
        ring.add_used(1, 16).unwrap();
        assert!(notify_used(&ring, &guard, true, true).unwrap());
    }

    #[test]
    fn notification_event_index_survives_u16_wraparound() {
        let (memory, ring) = queue();
        let guard = memory.memory();
        ring.set_queue_next_used(u16::MAX);
        guard
            .write_obj(u16::MAX.to_le(), GuestAddress(0x2014))
            .unwrap();
        ring.add_used(0, 16).unwrap();
        assert_eq!(used(&guard), 0);
        assert!(notify_used(&ring, &guard, true, true).unwrap());
    }

    #[test]
    fn active_poll_transport_consumes_notification_accounting_without_irq() {
        let (memory, ring) = queue();
        let guard = memory.memory();
        ring.add_used(0, 16).unwrap();
        assert!(!notify_used(&ring, &guard, true, false).unwrap());
        assert!(!ring.needs_notification().unwrap());
        guard
            .write_obj(1_u16.to_le(), GuestAddress(0x2014))
            .unwrap();
        ring.add_used(1, 16).unwrap();
        assert!(notify_used(&ring, &guard, true, true).unwrap());
    }

    fn quiet_test_backend(offered: bool) -> NvGpuBackend {
        NvGpuBackend {
            nvidia: Arc::new(Mutex::new(NvidiaBackend::with_default_zones())),
            mem: None,
            event_idx: true,
            quiet_arms_offered: offered,
            quiet_arms: false,
            control_batch: legion_quiet_arm::ControlBatch::default(),
            config: VirtioGpuNvConfig::default(),
            watches: None,
            poll_subscriptions: std::collections::HashSet::new(),
            event_wake: Arc::new(EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK).unwrap()),
            display_watch: false,
            req: Vec::new(),
            resp: vec![0; RESP_INITIAL],
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "backend fixture uses a real eventfd; pure wire/policy tests run under Miri"
    )]
    fn quiet_arm_feature_must_be_both_offered_and_acknowledged() {
        let mut backend = quiet_test_backend(false);
        assert_eq!(backend.features() & legion_quiet_arm::FEATURE, 0);
        backend.acked_features(legion_quiet_arm::FEATURE);
        assert!(!backend.quiet_arms);
        backend.quiet_arms_offered = true;
        assert_ne!(backend.features() & legion_quiet_arm::FEATURE, 0);
        backend.acked_features(0);
        assert!(!backend.quiet_arms);
        backend.acked_features(legion_quiet_arm::FEATURE);
        assert!(backend.quiet_arms);
        backend.acked_features(0);
        assert!(!backend.quiet_arms);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "backend fixture uses a real eventfd; pure wire/policy tests run under Miri"
    )]
    fn invalid_or_unnegotiated_quiet_arm_preserves_response_and_interrupt() {
        for negotiated in [false, true] {
            let (memory, ring) = queue();
            let guard = memory.memory();
            let mut backend = quiet_test_backend(true);
            backend.acked_features(if negotiated {
                legion_quiet_arm::FEATURE
            } else {
                0
            });
            guard
                .write_slice(&legion_quiet_arm::reply(97, 0), GuestAddress(0x4000))
                .unwrap();
            descriptor(&guard, 0, 0x4000, 16, 1, 1);
            descriptor(&guard, 1, 0x5000, 16, 2, 0);
            post(&guard, 0, 0);
            assert!(backend.process(&ring, &guard).unwrap());
            assert!(backend.control_batch.interrupt);
            assert!(backend.poll_subscriptions.is_empty());
            assert_eq!(used(&guard), 1);
            let status = guard.read_obj::<u32>(GuestAddress(0x5008)).unwrap();
            assert_eq!(
                u32::from_le(status) as i32,
                if negotiated {
                    -libc::EBADF
                } else {
                    -libc::EINVAL
                }
            );
        }
    }

    fn queue() -> (GuestMemoryAtomic<GuestMemoryMmap>, VringRwLock) {
        let mem = GuestMemoryAtomic::new(
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap(),
        );
        let vring = VringRwLock::new(mem.clone(), 8).unwrap();
        vring.set_queue_info(0x1000, 0x2000, 0x3000).unwrap();
        vring.set_queue_size(8);
        vring.set_queue_ready(true);
        vring.set_queue_event_idx(true);
        (mem, vring)
    }

    fn descriptor(mem: &GuestMemoryMmap, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut bytes = Vec::new();
        bytes.extend(addr.to_le_bytes());
        bytes.extend(len.to_le_bytes());
        bytes.extend(flags.to_le_bytes());
        bytes.extend(next.to_le_bytes());
        mem.write_slice(&bytes, GuestAddress(0x1000 + u64::from(index) * 16))
            .unwrap();
    }

    fn post(mem: &GuestMemoryMmap, index: u16, head: u16) {
        mem.write_obj(
            head.to_le(),
            GuestAddress(0x2004 + u64::from(index % 8) * 2),
        )
        .unwrap();
        mem.store(
            index.wrapping_add(1).to_le(),
            GuestAddress(0x2002),
            Ordering::Release,
        )
        .unwrap();
    }

    fn used(mem: &GuestMemoryMmap) -> u16 {
        u16::from_le(mem.load(GuestAddress(0x3002), Ordering::Acquire).unwrap())
    }

    #[test]
    fn event_queue_rearms_buffer_notifications_before_waiting() {
        let (mem, vring) = queue();
        let guard = mem.memory();
        for index in 0..8 {
            descriptor(&guard, index, 0x4000 + u64::from(index) * 16, 16, 2, 0);
            post(&guard, index, index);
            assert!(push_event(&vring, &mem, u32::from(index)));
        }
        let mut pending = PendingEvents::default();
        pending.mark(99);
        assert_eq!(pending.flush(|handle| push_event(&vring, &mem, handle)), 0);
        // EVENT_IDX's avail_event must follow the consumed index.
        let rearm = guard.read_obj::<u16>(GuestAddress(0x3044)).unwrap();
        assert_eq!(u16::from_le(rearm), 8);
        assert!(!pending.is_empty());
        post(&guard, 8, 0);
        assert_eq!(pending.flush(|handle| push_event(&vring, &mem, handle)), 1);
        assert!(pending.is_empty());
        assert_eq!(used(&guard), 9);
        let handle = guard.read_obj::<u32>(GuestAddress(0x4004)).unwrap();
        assert_eq!(u32::from_le(handle), 99);
    }

    #[test]
    #[cfg_attr(miri, ignore = "real eventfd readiness requires the native OS test")]
    fn event_queue_kicks_wake_the_pump_and_preserve_guest_buffers() {
        let (mem, vring) = queue();
        let guard = mem.memory();
        descriptor(&guard, 0, 0x4000, 16, 2, 0);
        post(&guard, 0, 0);
        let wake = Arc::new(EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK).unwrap());
        let mut backend = NvGpuBackend {
            nvidia: Arc::new(Mutex::new(NvidiaBackend::with_default_zones())),
            mem: None,
            event_idx: true,
            quiet_arms_offered: false,
            quiet_arms: false,
            control_batch: legion_quiet_arm::ControlBatch::default(),
            config: VirtioGpuNvConfig::default(),
            watches: None,
            poll_subscriptions: std::collections::HashSet::new(),
            event_wake: wake.clone(),
            display_watch: false,
            req: Vec::new(),
            resp: Vec::new(),
        };
        for _ in 0..2 {
            backend
                .handle_event(
                    EVENT_QUEUE as u16,
                    vmm_sys_util::epoll::EventSet::IN,
                    &[vring.clone(), vring.clone()],
                    0,
                )
                .unwrap();
        }
        assert_eq!(wake.read().unwrap(), 2);
        assert_eq!(
            wake.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(used(&guard), 0);
        // A saturated eventfd is already readable; the producer must neither block nor lose the wakeup by returning an EAGAIN failure.
        wake.write(u64::MAX - 1).unwrap();
        backend
            .handle_event(
                EVENT_QUEUE as u16,
                vmm_sys_util::epoll::EventSet::IN,
                &[vring.clone(), vring],
                0,
            )
            .unwrap();
        assert_eq!(wake.read().unwrap(), u64::MAX - 1);
        assert_eq!(used(&guard), 0);
    }

    #[test]
    fn event_ready_header_matches_wire_format() {
        for handle in [0u32, 7, u32::MAX] {
            let bytes = event_ready_bytes(handle);
            assert_eq!(bytes.len(), size_of::<MsgHeader>());
            let hdr = MsgHeader::ok(MsgType::EventReady, handle);
            // SAFETY: MsgHeader has four initialized u32 fields and no padding.
            let expected = unsafe {
                std::slice::from_raw_parts(
                    (&hdr as *const MsgHeader).cast::<u8>(),
                    size_of::<MsgHeader>(),
                )
            };
            assert_eq!(&bytes, expected);
        }
    }

    #[test]
    fn completion_payload_requires_full_buffer_and_survives_a_short_descriptor() {
        for length in [32_u32, 280] {
            let (mem, vring) = queue();
            let guard = mem.memory();
            let payload = vec![0x5a; length as usize];
            descriptor(&guard, 0, 0x4000, 16, 2, 0);
            post(&guard, 0, 0);
            assert!(!push_event_data(&vring, &mem, &payload, false));
            assert_eq!(used(&guard), 1);
            assert_eq!(guard.read_obj::<u32>(GuestAddress(0x3008)).unwrap(), 0);
            let mut untouched = [0; 16];
            guard
                .read_slice(&mut untouched, GuestAddress(0x4000))
                .unwrap();
            assert_eq!(untouched, [0; 16]);
            // The publisher retains its payload after false and retries intact.
            descriptor(&guard, 1, 0x5000, length, 2, 0);
            post(&guard, 1, 1);
            assert!(push_event_data(&vring, &mem, &payload, false));
            assert_eq!(used(&guard), 2);
            assert_eq!(guard.read_obj::<u32>(GuestAddress(0x3010)).unwrap(), length);
            let mut actual = vec![0; length as usize];
            guard.read_slice(&mut actual, GuestAddress(0x5000)).unwrap();
            assert_eq!(actual, payload);
        }
    }

    #[test]
    fn cached_requests_do_not_include_stale_bytes_and_join_readable_descriptors() {
        let (mem, vring) = queue();
        let mut backend = NvGpuBackend {
            nvidia: Arc::new(Mutex::new(NvidiaBackend::with_default_zones())),
            mem: Some(mem.clone()),
            event_idx: true,
            quiet_arms_offered: false,
            quiet_arms: false,
            control_batch: legion_quiet_arm::ControlBatch::default(),
            config: VirtioGpuNvConfig::default(),
            watches: None,
            poll_subscriptions: std::collections::HashSet::new(),
            event_wake: Arc::new(EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK).expect("test wake fd")),
            display_watch: false,
            req: Vec::new(),
            resp: vec![0; RESP_INITIAL],
        };
        let guard = mem.memory();
        // A malformed message, then a shorter header split across descriptors.
        for (index, len) in [4096usize, 16, 32, 0, 8192, 16]
            .into_iter()
            .cycle()
            .take(10000)
            .enumerate()
        {
            let mut request = vec![0xa5; len];
            if len >= 16 {
                let hdr = MsgHeader::ok(MsgType::Ioctl, index as u32);
                request[..16].copy_from_slice(unsafe {
                    std::slice::from_raw_parts((&hdr as *const MsgHeader).cast::<u8>(), 16)
                });
            }
            guard.write_slice(&request, GuestAddress(0x4000)).unwrap();
            let split = len / 2;
            descriptor(&guard, 0, 0x4000, split as u32, 1, 1);
            descriptor(&guard, 1, 0x4000 + split as u64, (len - split) as u32, 1, 2);
            descriptor(&guard, 2, 0x10000, 64, 2, 0);
            post(&guard, index as u16, 0);
            let mut expected = [0; 64];
            let n = backend
                .nvidia
                .lock()
                .unwrap()
                .dispatch(&request, &mut expected);
            assert!(backend.process(&vring, &guard).unwrap());
            assert_eq!(used(&guard), index as u16 + 1);
            assert_eq!(
                guard
                    .read_obj::<u32>(GuestAddress(0x3008 + (index % 8) as u64 * 8))
                    .unwrap(),
                n as u32
            );
            let mut actual = vec![0; n];
            guard
                .read_slice(&mut actual, GuestAddress(0x10000))
                .unwrap();
            assert_eq!(actual, expected[..n]);
        }
    }

    #[test]
    #[ignore = "request-copy benchmark; run in release mode on a fixed CPU"]
    fn request_queue_benchmark() {
        for len in [32, 4096, 65536] {
            let (mem, vring) = queue();
            let mut backend = NvGpuBackend {
                nvidia: Arc::new(Mutex::new(NvidiaBackend::with_default_zones())),
                mem: Some(mem.clone()),
                event_idx: true,
                quiet_arms_offered: false,
                quiet_arms: false,
                control_batch: legion_quiet_arm::ControlBatch::default(),
                config: VirtioGpuNvConfig::default(),
                watches: None,
                poll_subscriptions: std::collections::HashSet::new(),
                event_wake: Arc::new(
                    EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK).expect("test wake fd"),
                ),
                display_watch: false,
                req: Vec::new(),
                resp: vec![0; RESP_INITIAL],
            };
            let guard = mem.memory();
            let mut request = vec![0; len];
            request[..16].copy_from_slice(&event_ready_bytes(0));
            guard.write_slice(&request, GuestAddress(0x4000)).unwrap();
            descriptor(&guard, 0, 0x4000, len as u32, 1, 1);
            descriptor(&guard, 1, 0x18000, 16, 2, 0);
            for index in 0..100u16 {
                post(&guard, index, 0);
                assert!(backend.process(&vring, &guard).unwrap());
            }
            let start = Instant::now();
            for index in 100..1000100u32 {
                post(&guard, index as u16, 0);
                assert!(backend.process(&vring, &guard).unwrap());
            }
            assert_eq!(used(&guard), 1000100u32 as u16);
            println!(
                "REQUEST_QUEUE bytes={len} ns_per_call={:.1}",
                start.elapsed().as_nanos() as f64 / 1000000.0
            );
        }
    }
}
