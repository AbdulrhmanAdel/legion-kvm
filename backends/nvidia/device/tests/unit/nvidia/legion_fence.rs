use super::*;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, AtomicUsize, Ordering},
};

struct Host {
    calls: AtomicUsize,
    status: AtomicI32,
    raw: AtomicI32,
    peers: Mutex<Vec<UnixStream>>,
}

impl HostDriver for Arc<Host> {
    fn ioctl(&self, _: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match request {
            FENCE_CREATE => {
                let (file, peer) = UnixStream::pair().map_err(|_| libc::EIO)?;
                self.peers.lock().expect("peer lock").push(peer);
                use std::os::fd::IntoRawFd;
                let raw = file.into_raw_fd();
                self.raw.store(raw, Ordering::Relaxed);
                arg[16..20].copy_from_slice(&raw.to_le_bytes());
            }
            FILE_INFO => {
                arg[32..36].copy_from_slice(&self.status.load(Ordering::Relaxed).to_le_bytes())
            }
            FENCE_WAIT => {
                assert_eq!(arg.len(), 24);
                assert_eq!(word(arg, 0), 77);
                assert_eq!(word(arg, 4) as i32, self.raw.load(Ordering::Relaxed));
                assert_eq!(wide(arg, 8), 3);
                assert_eq!(wide(arg, 16), 4);
                // Verify that native producer errors reach the caller.
                return Err(libc::EIO);
            }
            _ => return Err(libc::ENOTTY),
        }
        Ok(())
    }
}

fn host() -> Arc<Host> {
    Arc::new(Host {
        calls: AtomicUsize::new(0),
        status: AtomicI32::new(0),
        raw: AtomicI32::new(-1),
        peers: Mutex::new(Vec::new()),
    })
}

fn many_packet(tokens: &[u64]) -> [u8; WAIT_MANY_BYTES] {
    let mut packet = [0_u8; WAIT_MANY_BYTES];
    packet[..4].copy_from_slice(&77_u32.to_le_bytes());
    packet[4..8].copy_from_slice(&(tokens.len() as u32).to_le_bytes());
    packet[8..16].copy_from_slice(&3_u64.to_le_bytes());
    packet[16..24].copy_from_slice(&4_u64.to_le_bytes());
    for (i, token) in tokens.iter().enumerate() {
        packet[32 + i * 8..40 + i * 8].copy_from_slice(&token.to_le_bytes());
    }
    packet
}

#[test]
fn pure_many_wait_checks_count_padding_lengths_and_owner_before_native_calls() {
    let host = host();
    let fences = Fences::default();
    let packet = many_packet(&[1, 2]);
    for size in 0..WAIT_MANY_BYTES {
        assert_eq!(validate_wait_many(&packet[..size]), Err(libc::EINVAL));
    }
    assert_eq!(validate_wait_many(&packet), Ok(2));
    assert_eq!(
        validate_wait_many(&many_packet(&[1; MAX_WAIT_INPUTS])),
        Ok(MAX_WAIT_INPUTS)
    );
    for count in [0_u32, 33, u32::MAX] {
        let mut bad = packet;
        bad[4..8].copy_from_slice(&count.to_le_bytes());
        assert_eq!(validate_wait_many(&bad), Err(libc::EINVAL));
    }
    for offset in [16, 24, 48, WAIT_MANY_BYTES - 1] {
        let mut bad = packet;
        bad[offset] = 0xff;
        if offset == 16 {
            bad[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        }
        assert_eq!(validate_wait_many(&bad), Err(libc::EINVAL));
    }
    assert_eq!(fences.wait_many(&host, 7, -1, &packet), Err(libc::ENOENT));
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

struct MissingMerge;
impl HostDriver for MissingMerge {
    fn ioctl(&self, _: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        assert_eq!(request, SYNC_MERGE);
        assert_eq!(arg.len(), 48);
        assert_eq!(word(arg, 36) as i32, -1);
        Ok(())
    }
}

#[test]
fn pure_merge_success_without_owned_descriptor_is_rejected() {
    assert_eq!(merge_native(&MissingMerge, -1, -1).err(), Some(libc::EIO));
}

struct MergeHost {
    source: Arc<Host>,
    pairs: Mutex<Vec<(RawFd, RawFd)>>,
    peers: Mutex<Vec<UnixStream>>,
    last: AtomicI32,
    waits: AtomicUsize,
    fail_merge: usize,
    fail_wait: bool,
}

impl HostDriver for MergeHost {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        match request {
            SYNC_MERGE => {
                let mut pairs = self.pairs.lock().expect("pairs");
                if !pairs.is_empty() {
                    assert_eq!(fd, self.last.load(Ordering::Relaxed));
                }
                pairs.push((fd, word(arg, 32) as i32));
                if pairs.len() == self.fail_merge {
                    return Err(libc::ENOSPC);
                }
                let (file, peer) = UnixStream::pair().map_err(|_| libc::EIO)?;
                self.peers.lock().expect("peers").push(peer);
                use std::os::fd::IntoRawFd;
                let raw = file.into_raw_fd();
                self.last.store(raw, Ordering::Relaxed);
                arg[36..40].copy_from_slice(&raw.to_le_bytes());
                Ok(())
            }
            FENCE_WAIT => {
                self.waits.fetch_add(1, Ordering::Relaxed);
                assert_eq!(word(arg, 0), 77);
                assert_eq!(word(arg, 4) as i32, self.last.load(Ordering::Relaxed));
                assert_eq!((wide(arg, 8), wide(arg, 16)), (3, 4));
                if self.fail_wait {
                    Err(libc::EIO)
                } else {
                    Ok(())
                }
            }
            _ => self.source.ioctl(fd, request, arg),
        }
    }
}

#[test]
fn merged_waits_translate_tokens_and_release_temporaries_on_success_and_failures() {
    for (fail_merge, fail_wait, expected) in [
        (0, false, Ok(())),
        (2, false, Err(libc::ENOSPC)),
        (0, true, Err(libc::EIO)),
    ] {
        let source = host();
        let host = MergeHost {
            source: source.clone(),
            pairs: Mutex::new(Vec::new()),
            peers: Mutex::new(Vec::new()),
            last: AtomicI32::new(-1),
            waits: AtomicUsize::new(0),
            fail_merge,
            fail_wait,
        };
        let mut fences = Fences::default();
        let tokens: Vec<_> = (0..3)
            .map(|_| fences.export(&host, 7, -1, &[0; 32]).expect("export"))
            .collect();
        let raw: Vec<_> = tokens
            .iter()
            .map(|id| fences.live[&(7, *id)].fd.as_raw_fd())
            .collect();
        let packet = many_packet(&tokens);
        assert_eq!(fences.wait_many(&host, 8, -1, &packet), Err(libc::ENOENT));
        let mut stale = packet;
        stale[48..56].copy_from_slice(&999_u64.to_le_bytes());
        assert_eq!(fences.wait_many(&host, 7, -1, &stale), Err(libc::ENOENT));
        assert!(host.pairs.lock().expect("pairs").is_empty());
        assert_eq!(fences.wait_many(&host, 7, -1, &packet), expected);
        let pairs = host.pairs.lock().expect("pairs");
        assert_eq!(pairs[0], (raw[0], raw[1]));
        assert_eq!(pairs[1].1, raw[2]);
        assert_eq!(
            host.waits.load(Ordering::Relaxed),
            usize::from(fail_merge == 0)
        );
        for peer in host.peers.lock().expect("peers").iter_mut() {
            peer.set_nonblocking(true).expect("nonblocking");
            assert_eq!(peer.read(&mut [0]).expect("temporary closed"), 0);
        }
        for peer in source.peers.lock().expect("source peers").iter_mut() {
            peer.set_nonblocking(true).expect("nonblocking");
            assert_eq!(
                peer.read(&mut [0]).expect_err("input still owned").kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        fences.close_owner(7);
        for peer in source.peers.lock().expect("source peers").iter_mut() {
            assert_eq!(peer.read(&mut [0]).expect("source closed"), 0);
        }
    }
}

struct ContextHost(Arc<AtomicUsize>);

fn completed_packet() -> [u8; 24] {
    let mut p = [0_u8; 24];
    p[..4].copy_from_slice(&77_u32.to_le_bytes());
    p[8..16].copy_from_slice(&3_u64.to_le_bytes());
    p[16..].copy_from_slice(&4_u64.to_le_bytes());
    p
}
#[test]
fn pure_completed_wait_checks_layout_before_native_requests() {
    let host = host();
    let p = completed_packet();
    assert_eq!(validate_completed_wait(&p), Ok(()));
    for size in 0..24 {
        assert_eq!(completed_wait(&host, -1, &p[..size]), Err(libc::EINVAL));
    }
    for (offset, value) in [(0, 0), (4, 1), (8, 4), (8, 5)] {
        let mut bad = p;
        bad[offset] = value;
        assert_eq!(completed_wait(&host, -1, &bad), Err(libc::EINVAL));
    }
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}
struct MissingReady;
impl HostDriver for MissingReady {
    fn ioctl(&self, _: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        assert_eq!(request, FENCE_CREATE);
        assert_eq!(arg.len(), 24);
        assert_eq!((word(arg, 0), word(arg, 4), wide(arg, 8)), (77, 0, 0));
        assert_eq!(word(arg, 16) as i32, -1);
        Ok(())
    }
}
#[test]
fn pure_completed_wait_without_native_descriptor_is_rejected() {
    assert_eq!(
        completed_wait(&MissingReady, -1, &completed_packet()),
        Err(libc::EIO)
    );
}
struct ReadyHost {
    peers: Mutex<Vec<UnixStream>>,
    raw: AtomicI32,
    status: i32,
    fail: u64,
    waits: AtomicUsize,
}
impl HostDriver for ReadyHost {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        if request == self.fail {
            return Err(libc::ENOMEM);
        }
        match request {
            FENCE_CREATE => {
                assert_eq!((word(arg, 0), word(arg, 4), wide(arg, 8)), (77, 0, 0));
                let (file, peer) = UnixStream::pair().map_err(|_| libc::EIO)?;
                self.peers.lock().expect("peers").push(peer);
                use std::os::fd::IntoRawFd;
                let raw = file.into_raw_fd();
                self.raw.store(raw, Ordering::Relaxed);
                arg[16..20].copy_from_slice(&raw.to_le_bytes());
                Ok(())
            }
            FILE_INFO => {
                assert_eq!(fd, self.raw.load(Ordering::Relaxed));
                arg[32..36].copy_from_slice(&self.status.to_le_bytes());
                Ok(())
            }
            FENCE_WAIT => {
                self.waits.fetch_add(1, Ordering::Relaxed);
                assert_eq!(word(arg, 0), 77);
                assert_eq!(word(arg, 4) as i32, self.raw.load(Ordering::Relaxed));
                assert_eq!((wide(arg, 8), wide(arg, 16)), (3, 4));
                Ok(())
            }
            _ => Err(libc::ENOTTY),
        }
    }
}
#[test]
fn completed_wait_verifies_native_readiness_and_closes_fd_on_all_paths() {
    for (status, fail, expected, waits) in [
        (1, 0, Ok(()), 1),
        (0, 0, Err(libc::EIO), 0),
        (-110, 0, Err(libc::EIO), 0),
        (1, FENCE_CREATE, Err(libc::ENOMEM), 0),
        (1, FILE_INFO, Err(libc::ENOMEM), 0),
        (1, FENCE_WAIT, Err(libc::ENOMEM), 0),
    ] {
        let host = ReadyHost {
            peers: Mutex::new(Vec::new()),
            raw: AtomicI32::new(-1),
            status,
            fail,
            waits: AtomicUsize::new(0),
        };
        assert_eq!(completed_wait(&host, -1, &completed_packet()), expected);
        assert_eq!(host.waits.load(Ordering::Relaxed), waits);
        for peer in host.peers.lock().expect("peers").iter_mut() {
            peer.set_nonblocking(true).expect("nonblocking");
            assert_eq!(peer.read(&mut [0]).expect("temporary closed"), 0);
        }
    }
}

#[test]
fn pure_attachment_validates_packet_and_scope_before_native_calls() {
    let host = host();
    let fences = Fences::default();
    let mut packet = [0_u8; 24];
    packet[0] = 88;
    packet[8] = 1;
    for size in 0..24 {
        assert_eq!(validate_attach(&packet[..size]), Err(libc::EINVAL));
    }
    assert_eq!(validate_attach(&packet), Ok(()));
    for (offset, value) in [(0, 0), (4, 2), (16, 1)] {
        let mut bad = packet;
        bad[offset] = value;
        assert_eq!(fences.attach(&host, 7, -1, &bad), Err(libc::EINVAL));
    }
    assert_eq!(fences.attach(&host, 7, -1, &packet), Err(libc::ENOENT));
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

struct MissingBuffer;
impl HostDriver for MissingBuffer {
    fn ioctl(&self, _: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        assert_eq!(request, PRIME_EXPORT);
        assert_eq!(arg.len(), 12);
        assert_eq!(
            (word(arg, 0), word(arg, 4), word(arg, 8) as i32),
            (88, (libc::O_CLOEXEC | libc::O_RDWR) as u32, -1)
        );
        Ok(())
    }
}
#[test]
fn pure_buffer_export_without_owned_descriptor_is_rejected() {
    assert_eq!(export_buffer(&MissingBuffer, -1, 88).err(), Some(libc::EIO));
}

struct AttachHost {
    source: Arc<Host>,
    peers: Mutex<Vec<UnixStream>>,
    buffer: AtomicI32,
    shared: bool,
    fail: bool,
}
impl HostDriver for AttachHost {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        match request {
            PRIME_EXPORT => {
                assert_eq!(word(arg, 0), 88);
                let (file, peer) = UnixStream::pair().map_err(|_| libc::EIO)?;
                self.peers.lock().expect("peers").push(peer);
                use std::os::fd::IntoRawFd;
                let raw = file.into_raw_fd();
                self.buffer.store(raw, Ordering::Relaxed);
                arg[8..].copy_from_slice(&raw.to_le_bytes());
                Ok(())
            }
            IMPORT_SYNC_FILE => {
                assert_eq!(fd, self.buffer.load(Ordering::Relaxed));
                assert_eq!(word(arg, 0), if self.shared { 1 } else { 2 });
                assert_eq!(word(arg, 4) as i32, self.source.raw.load(Ordering::Relaxed));
                if self.fail { Err(libc::EIO) } else { Ok(()) }
            }
            _ => self.source.ioctl(fd, request, arg),
        }
    }
}
#[test]
fn attachment_maps_usage_preserves_fence_and_closes_buffer_fd_even_on_failure() {
    for shared in [false, true] {
        for fail in [false, true] {
            let host = AttachHost {
                source: host(),
                peers: Mutex::new(Vec::new()),
                buffer: AtomicI32::new(-1),
                shared,
                fail,
            };
            let mut fences = Fences::default();
            let token = fences.export(&host, 7, -1, &[0; 32]).expect("export");
            let mut packet = [0_u8; 24];
            packet[..4].copy_from_slice(&88_u32.to_le_bytes());
            packet[4..8].copy_from_slice(&u32::from(shared).to_le_bytes());
            packet[8..16].copy_from_slice(&token.to_le_bytes());
            assert_eq!(fences.attach(&host, 8, -1, &packet), Err(libc::ENOENT));
            assert!(host.peers.lock().expect("peers").is_empty());
            assert_eq!(
                fences.attach(&host, 7, -1, &packet),
                if fail { Err(libc::EIO) } else { Ok(()) }
            );
            let mut peers = host.peers.lock().expect("peers");
            peers[0].set_nonblocking(true).expect("nonblocking");
            assert_eq!(peers[0].read(&mut [0]).expect("closed buffer"), 0);
            let mut source = host.source.peers.lock().expect("source");
            source[0].set_nonblocking(true).expect("nonblocking");
            assert_eq!(
                source[0]
                    .read(&mut [0])
                    .expect_err("fence still owned")
                    .kind(),
                std::io::ErrorKind::WouldBlock
            );
            fences.close_owner(7);
            assert_eq!(source[0].read(&mut [0]).expect("closed fence"), 0);
        }
    }
}

impl HostDriver for ContextHost {
    fn ioctl(&self, _: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        assert_eq!(request, CONTEXT_CREATE);
        assert_eq!(arg.len(), 32);
        assert_eq!(wide(arg, 16), 16);
        let pointer = wide(arg, 8) as *const u8;
        // SAFETY: handle_fence_context owns a live 16-byte array through this synchronous mock call and replaces only its outer pointer.
        let nested = unsafe { std::slice::from_raw_parts(pointer, 16) };
        assert_eq!(word(nested, 0), 0x1234);
        assert_eq!(word(nested, 4), 0x5678);
        assert_eq!(wide(nested, 8), 4096);
        self.0.fetch_add(1, Ordering::Relaxed);
        arg[24..28].copy_from_slice(&77_u32.to_le_bytes());
        Ok(())
    }
}

fn context_packet() -> [u8; 48] {
    let mut packet = [0; 48];
    packet[8..16].copy_from_slice(&0xf00d_dead_beef_u64.to_le_bytes());
    packet[16..24].copy_from_slice(&16_u64.to_le_bytes());
    packet[32..36].copy_from_slice(&0x1234_u32.to_le_bytes());
    packet[36..40].copy_from_slice(&0x5678_u32.to_le_bytes());
    packet[40..].copy_from_slice(&4096_u64.to_le_bytes());
    packet
}

struct LayoutHost {
    created: Arc<AtomicUsize>,
    device_parent: bool,
    mismatch: bool,
    rm_error: bool,
}

impl HostDriver for LayoutHost {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        match request {
            RM_CONTROL => {
                assert_eq!(arg.len(), 32);
                assert_eq!(word(arg, 0), 0x1234);
                if self.rm_error {
                    arg[28..].copy_from_slice(&0x23_u32.to_le_bytes());
                    return Ok(());
                }
                let command = word(arg, 8);
                let len = word(arg, 24) as usize;
                let pointer = wide(arg, 16) as *mut u8;
                // SAFETY: native_control owns the exact declared live buffer, uniquely borrows it through this synchronous mock call and does not access it until we return.
                let data = unsafe { std::slice::from_raw_parts_mut(pointer, len) };
                match command {
                    HANDLE_INFO => {
                        assert_eq!(data.len(), 16);
                        let object = word(data, 0);
                        let index = word(data, 4);
                        let result: u32 = match (object, index) {
                            (0x5678, 2) => 0xda,
                            (0x5678, 1) => 0x2222,
                            (0x2222, 2) if self.device_parent => 0x80,
                            (0x2222 | 0x3333, 2) => 0x2080,
                            _ => panic!("unexpected object lookup"),
                        };
                        data[8..12].copy_from_slice(&result.to_le_bytes());
                    }
                    CHILD_HANDLE => {
                        assert_eq!(data.len(), 12);
                        assert_eq!(word(data, 0), 0x2222);
                        assert_eq!(word(data, 4), 0x2080);
                        data[8..].copy_from_slice(&0x3333_u32.to_le_bytes());
                    }
                    GPU_ID => data.copy_from_slice(&0x100_u32.to_le_bytes()),
                    SURFACE_LAYOUT => {
                        assert_eq!(data.len(), 32);
                        data[..8].copy_from_slice(&24_u64.to_le_bytes());
                        data[8..16].copy_from_slice(&16_u64.to_le_bytes());
                        data[16..24].copy_from_slice(&32_u64.to_le_bytes());
                        data[24..28].copy_from_slice(&3_u32.to_le_bytes());
                    }
                    _ => panic!("unexpected RM command"),
                }
                arg[28..].fill(0);
                Ok(())
            }
            0xc024_6443 => {
                let gpu: u32 = if self.mismatch { 0x101 } else { 0x100 };
                arg[..4].copy_from_slice(&gpu.to_le_bytes());
                Ok(())
            }
            CONTEXT_CREATE => ContextHost(self.created.clone()).ioctl(fd, request, arg),
            _ => Err(libc::ENOTTY),
        }
    }
}

#[test]
fn pure_surface_layout_checks_stride_offsets_capacity_and_overflow() {
    let layout = SurfaceLayout {
        stride: 32,
        maximum: 24,
        monitored: 16,
        caps: 3,
    };
    assert_eq!(layout.validate(1, 4096), Ok(()));
    assert_eq!(layout.validate(127, 4096), Ok(()));
    for (index, size) in [
        (128, 4096),
        (1, 63),
        (u64::MAX, 4096),
        (u64::MAX - 1, u64::MAX),
    ] {
        assert_eq!(layout.validate(index, size), Err(libc::EINVAL));
    }
    for stride in [0, 7, 12] {
        assert_eq!(
            SurfaceLayout { stride, ..layout }.validate(1, 4096),
            Err(libc::EOPNOTSUPP)
        );
    }
    for maximum in [25, 32, u64::MAX] {
        assert_eq!(
            SurfaceLayout { maximum, ..layout }.validate(1, 4096),
            Err(libc::EINVAL)
        );
    }
    assert_eq!(
        SurfaceLayout {
            monitored: 32,
            ..layout
        }
        .validate(1, 4096),
        Err(libc::EINVAL)
    );
    assert_eq!(
        SurfaceLayout { caps: 4, ..layout }.validate(1, 4096),
        Err(libc::EOPNOTSUPP)
    );
}

#[test]
fn pure_native_layout_queries_use_owned_buffers_and_check_rm_status() {
    let mut host = LayoutHost {
        created: Arc::new(AtomicUsize::new(0)),
        device_parent: false,
        mismatch: false,
        rm_error: false,
    };
    assert_eq!(handle_info(&host, -1, 0x1234, 0x5678, 2), Ok(0xda));
    assert_eq!(handle_info(&host, -1, 0x1234, 0x5678, 1), Ok(0x2222));
    let mut layout = [0_u8; 32];
    native_control(&host, -1, 0x1234, 0x2222, SURFACE_LAYOUT, &mut layout).expect("layout");
    assert_eq!((wide(&layout, 16), word(&layout, 24)), (32, 3));
    host.rm_error = true;
    assert_eq!(handle_info(&host, -1, 0x1234, 0x5678, 2), Err(libc::EINVAL));
}

#[test]
fn nonzero_context_queries_runtime_layout_and_resolves_device_parent() {
    for device_parent in [false, true] {
        let created = Arc::new(AtomicUsize::new(0));
        let mut backend = NvidiaBackend::for_test();
        backend.legion_fences.enabled = true;
        backend.host = Box::new(LayoutHost {
            created: created.clone(),
            device_parent,
            mismatch: false,
            rm_error: false,
        });
        let control = backend.handles.insert(
            std::fs::File::open("/dev/null")
                .expect("mock control")
                .into(),
        );
        backend.handle_kinds.insert(control, DeviceKind::Ctl);
        let mut packet = context_packet();
        let mut response = [0_u8; 128];
        for index in [1_u64, 127] {
            packet[..8].copy_from_slice(&index.to_le_bytes());
            backend.handle_fence_context(0, -1, &packet, &mut response);
            assert_eq!(word(&response, 8), 0);
        }
        assert_eq!(created.load(Ordering::Relaxed), 2);
    }
}

#[test]
fn nonzero_context_refuses_bounds_gpu_mismatch_and_rm_status_before_creation() {
    for (index, mismatch, rm_error, expected) in [
        (128_u64, false, false, libc::EINVAL),
        (u64::MAX, false, false, libc::EINVAL),
        (1, true, false, libc::EXDEV),
        (1, false, true, libc::EOPNOTSUPP),
    ] {
        let created = Arc::new(AtomicUsize::new(0));
        let mut backend = NvidiaBackend::for_test();
        backend.legion_fences.enabled = true;
        backend.host = Box::new(LayoutHost {
            created: created.clone(),
            device_parent: false,
            mismatch,
            rm_error,
        });
        let control = backend.handles.insert(
            std::fs::File::open("/dev/null")
                .expect("mock control")
                .into(),
        );
        backend.handle_kinds.insert(control, DeviceKind::Ctl);
        let mut packet = context_packet();
        packet[..8].copy_from_slice(&index.to_le_bytes());
        let mut response = [0_u8; 128];
        backend.handle_fence_context(0, -1, &packet, &mut response);
        assert_eq!(word(&response, 8) as i32, -expected);
        assert_eq!(created.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn pure_context_lengths_are_checked_before_pointer_fields() {
    let packet = context_packet();
    assert_eq!(validate_context(&packet, 128), Ok(()));
    for size in 0..48 {
        assert_eq!(validate_context(&packet[..size], 128), Err(libc::EINVAL));
    }
    let mut indexed = packet;
    indexed[0] = 1;
    assert_eq!(validate_context(&indexed, 128), Ok(()));
    assert_eq!(validate_context(&packet, 16), Err(libc::ENOSPC));
}

#[test]
fn pure_empty_registry_and_overflow_do_not_issue_host_requests() {
    let host = host();
    let mut fences = Fences::default();
    assert_eq!(fences.status(&host, 1, 1), Err(libc::ENOENT));
    fences.close_owner(1);
    fences.clear();
    fences.next = u64::MAX;
    assert_eq!(fences.export(&host, 1, -1, &[0; 32]), Err(libc::EOVERFLOW));
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
    fences.next = (1_u64 << 63) - 1;
    assert_eq!(fences.export(&host, 1, -1, &[0; 32]), Err(libc::EOVERFLOW));
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn pure_wait_rejects_truncation_reserved_fields_and_nonmonotonic_values() {
    let host = host();
    let fences = Fences::default();
    let mut packet = [0_u8; 32];
    packet[24] = 1;
    for len in 0..32 {
        assert_eq!(fences.wait(&host, 1, -1, &packet[..len]), Err(libc::EINVAL));
    }
    assert_eq!(fences.wait(&host, 1, -1, &packet), Err(libc::ENOENT));
    packet[4] = 1;
    assert_eq!(fences.wait(&host, 1, -1, &packet), Err(libc::EINVAL));
    packet[4] = 0;
    packet[16] = 1;
    assert_eq!(fences.wait(&host, 1, -1, &packet), Err(libc::EINVAL));
    packet[16] = 2;
    assert_eq!(fences.wait(&host, 1, -1, &packet), Err(libc::EINVAL));
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn wait_translates_only_owned_live_tokens_and_preserves_host_error() {
    let host = host();
    let mut fences = Fences::default();
    let token = fences.export(&host, 7, -1, &[0; 32]).expect("export");
    let mut packet = [0_u8; 32];
    packet[..4].copy_from_slice(&77_u32.to_le_bytes());
    packet[8..16].copy_from_slice(&token.to_le_bytes());
    packet[16..24].copy_from_slice(&3_u64.to_le_bytes());
    packet[24..].copy_from_slice(&4_u64.to_le_bytes());
    let before = host.calls.load(Ordering::Relaxed);
    assert_eq!(fences.wait(&host, 8, -1, &packet), Err(libc::ENOENT));
    assert_eq!(host.calls.load(Ordering::Relaxed), before);
    assert_eq!(fences.wait(&host, 7, -1, &packet), Err(libc::EIO));
    assert_eq!(host.calls.load(Ordering::Relaxed), before + 1);
    fences.close((7, token));
    assert_eq!(fences.wait(&host, 7, -1, &packet), Err(libc::ENOENT));
    assert_eq!(host.calls.load(Ordering::Relaxed), before + 1);
}

#[test]
fn wait_dispatch_checks_gate_layout_and_render_owner_before_host_calls() {
    let host = host();
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    backend.current_handle = 7;
    backend.legion_fences.enabled = false;
    let mut response = [0_u8; 128];
    let mut input = [0_u8; 32];
    input[24] = 1;
    backend.handle_fence_probe(0, -1, WAIT, &input, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::EOPNOTSUPP);
    backend.legion_fences.enabled = true;
    backend.handle_fence_probe(0, -1, WAIT, &input, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::ENOTTY);
    backend.handle_kinds.insert(7, DeviceKind::Dri(0));
    backend.handle_fence_probe(0, -1, WAIT, &input[..31], &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    backend.handle_fence_probe(0, -1, WAIT, &input, &mut response[..16]);
    assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    backend.handle_fence_probe(0, -1, WAIT, &input, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::ENOENT);
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn pure_export_failure_does_not_allocate_a_token_or_resource() {
    struct Refusal;
    impl HostDriver for Refusal {
        fn ioctl(&self, _: RawFd, _: u64, _: &mut [u8]) -> std::result::Result<(), i32> {
            Err(libc::EACCES)
        }
    }
    let mut fences = Fences::default();
    assert_eq!(fences.export(&Refusal, 1, -1, &[0; 32]), Err(libc::EACCES));
    assert_eq!(fences.next, 1);
    assert!(fences.live.is_empty());
}

#[test]
fn export_status_preserves_pending_success_and_error_without_arming() {
    for status in [0, 1, -libc::ETIMEDOUT] {
        let host = host();
        host.status.store(status, Ordering::Relaxed);
        let mut fences = Fences::default();
        let (token, actual) = fences
            .export_status(&host, 7, -1, &[0; 32])
            .expect("checked export");
        assert_eq!(actual, status);
        assert_eq!(fences.live.len(), 1);
        assert_eq!(fences.metrics, [1, 0, 0, 0]);
        assert!(fences.added.is_empty());
        assert_eq!(fences.status(&host, 8, token), Err(libc::ENOENT));
        if status == 0 {
            // Becoming ready between the reply and ARM must still use normal completion; the snapshot never authorizes success.
            host.status.store(1, Ordering::Relaxed);
            fences.arm(7, token).expect("arm pending export");
            assert_eq!(fences.added.len(), 1);
        }
        fences.close((7, token)).expect("retained token");
        assert!(fences.live.is_empty());
    }
}

#[test]
fn export_status_query_failure_or_invalid_status_releases_native_descriptor() {
    struct QueryHost {
        source: Arc<Host>,
        fail: bool,
    }
    impl HostDriver for QueryHost {
        fn ioctl(&self, fd: RawFd, cmd: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
            if self.fail && cmd == FILE_INFO {
                Err(libc::EIO)
            } else {
                self.source.ioctl(fd, cmd, arg)
            }
        }
    }
    for (status, fail, expected) in [
        (0, true, libc::EIO),
        (2, false, libc::EPROTO),
        (-4096, false, libc::EPROTO),
    ] {
        let source = host();
        source.status.store(status, Ordering::Relaxed);
        let host = QueryHost {
            source: source.clone(),
            fail,
        };
        let mut fences = Fences::default();
        assert_eq!(fences.export_status(&host, 7, -1, &[0; 32]), Err(expected));
        assert!(fences.live.is_empty() && fences.added.is_empty());
        assert_eq!(fences.next, 2); // failed token cannot be reused
        let mut peers = source.peers.lock().expect("peers");
        peers[0].set_nonblocking(true).expect("nonblocking");
        assert_eq!(peers[0].read(&mut [0]).expect("descriptor closed"), 0);
    }
}

#[test]
fn export_status_rejects_gate_owner_lengths_and_reserved_fields_before_native_calls() {
    let host = host();
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    backend.current_handle = 7;
    backend.legion_fences.enabled = false;
    let mut response = [0_u8; 128];
    let input = [0_u8; 32];
    backend.handle_fence_probe(0, -1, EXPORT_STATUS, &input, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::EOPNOTSUPP);
    backend.legion_fences.enabled = true;
    backend.handle_fence_probe(0, -1, EXPORT_STATUS, &input, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::ENOTTY);
    backend.handle_kinds.insert(7, DeviceKind::Dri(0));
    for size in 0..32 {
        backend.handle_fence_probe(0, -1, EXPORT_STATUS, &input[..size], &mut response);
        assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    }
    for offset in [16, 24, 28] {
        let mut bad = input;
        bad[offset] = 1;
        backend.handle_fence_probe(0, -1, EXPORT_STATUS, &bad, &mut response);
        assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    }
    backend.handle_fence_probe(0, -1, EXPORT_STATUS, &input, &mut response[..16]);
    assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn pure_export_status_checks_lengths_padding_and_status_range() {
    let input = [0_u8; 33];
    for size in 0..=33 {
        assert_eq!(
            validate_export(&input[..size]),
            if size == 32 {
                Ok(())
            } else {
                Err(libc::EINVAL)
            }
        );
    }
    for offset in 16..32 {
        let mut input = [0_u8; 32];
        input[offset] = 1;
        assert_eq!(validate_export(&input), Err(libc::EINVAL));
    }
    for status in [-4095, -libc::ETIMEDOUT, -1, 0, 1] {
        assert_eq!(validate_export_status(status), Ok(status));
    }
    for status in [i32::MIN, -4096, 2, i32::MAX] {
        assert_eq!(validate_export_status(status), Err(libc::EPROTO));
    }
}

#[test]
fn export_status_dispatch_encodes_signed_terminal_status_and_retains_token() {
    let host = host();
    host.status.store(-libc::ETIMEDOUT, Ordering::Relaxed);
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    backend.current_handle = 7;
    backend.legion_fences.enabled = true;
    backend.handle_kinds.insert(7, DeviceKind::Dri(0));
    let mut response = [0_u8; 128];
    let prefix = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    let n = backend.handle_fence_probe(0, -1, EXPORT_STATUS, &[0; 32], &mut response);
    assert_eq!(n, prefix + 32);
    assert_eq!(word(&response, 8), 0);
    let token = wide(&response, prefix + 16);
    assert_eq!(word(&response, prefix + 24) as i32, -libc::ETIMEDOUT);
    assert_eq!(word(&response, prefix + 28), 0);
    assert!(backend.legion_fences.live.contains_key(&(7, token)));
    assert_eq!(backend.legion_fences.metrics, [1, 0, 0, 0]);
}

#[test]
fn pure_export_arm_validates_cookie_lengths_and_reserved_words() {
    for size in 0..=33 {
        assert_eq!(validate_export_arm(&[0; 33][..size]), Err(libc::EINVAL));
    }
    let mut packet = [0_u8; 32];
    for cookie in [1_u64, (1 << 32) + 1, (1 << 63) - 1] {
        packet[16..24].copy_from_slice(&cookie.to_le_bytes());
        assert_eq!(validate_export_arm(&packet), Ok(cookie));
    }
    for offset in 24..32 {
        let mut bad = packet;
        bad[offset] = 1;
        assert_eq!(validate_export_arm(&bad), Err(libc::EINVAL));
    }
    for cookie in [0_u64, 1 << 63, u64::MAX] {
        packet[16..24].copy_from_slice(&cookie.to_le_bytes());
        assert_eq!(validate_export_arm(&packet), Err(libc::EINVAL));
    }
}

#[test]
fn combined_export_arms_once_and_echoes_cookie_without_using_it_as_native_token() {
    let host = host();
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    backend.current_handle = 7;
    backend.legion_fences.enabled = true;
    backend.handle_kinds.insert(7, DeviceKind::Dri(0));
    let cookie = (1_u64 << 32) + 17;
    let mut packet = [0_u8; 32];
    packet[16..24].copy_from_slice(&cookie.to_le_bytes());
    let mut reply = [0; 128];
    let prefix = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    backend.handle_fence_probe(0, -1, EXPORT_ARM, &packet, &mut reply);
    assert_eq!(word(&reply, 8), 0);
    let token = wide(&reply, prefix + 16);
    assert_ne!(token, cookie);
    assert_eq!(backend.legion_fences.metrics, [1, 1, 0, 0]);
    assert!(backend.prepare_legion_fence_event(token).is_none());
    host.status.store(1, Ordering::Relaxed);
    let event = backend
        .prepare_legion_fence_event(token)
        .expect("completion");
    assert_eq!(word(&event, 4), 7);
    assert_eq!(wide(&event, 16), token);
    assert_eq!(wide(&event, 24), cookie);
    let calls = host.calls.load(Ordering::Relaxed);
    assert_eq!(
        backend.legion_fences.export_arm(&host, 7, -1, &packet),
        Err(libc::EEXIST)
    );
    assert_eq!(host.calls.load(Ordering::Relaxed), calls);
    let other = backend
        .legion_fences
        .export_arm(&host, 8, -1, &packet)
        .expect("other owner");
    assert_ne!(other, token);
    backend.legion_fences.close_owner(7);
    assert!(backend.prepare_legion_fence_event(token).is_none());
    assert!(backend.legion_fences.live.contains_key(&(8, other)));
    host.status.store(-libc::ETIMEDOUT, Ordering::Relaxed);
    let event = backend
        .prepare_legion_fence_event(other)
        .expect("producer error");
    assert_eq!(word(&event, 8) as i32, -libc::ETIMEDOUT);
    backend.legion_fences.close_owner(8);
    assert!(backend.legion_fences.live.is_empty());
    assert!(backend.legion_fences.added.is_empty());
}

#[test]
fn pure_combined_export_native_failure_keeps_no_token_or_watch() {
    struct Refusal;
    impl HostDriver for Refusal {
        fn ioctl(&self, _: RawFd, _: u64, _: &mut [u8]) -> std::result::Result<(), i32> {
            Err(libc::EACCES)
        }
    }
    let mut fences = Fences::default();
    let mut packet = [0_u8; 32];
    packet[16] = 1;
    assert_eq!(
        fences.export_arm(&Refusal, 7, -1, &packet),
        Err(libc::EACCES)
    );
    assert!(fences.live.is_empty() && fences.added.is_empty());
    assert_eq!(fences.next, 1);
    assert_eq!(fences.metrics, [0; 4]);
}

#[test]
fn accepted_event_is_counted_once_when_close_retires_token_during_publication() {
    let host = host();
    host.status.store(1, Ordering::Relaxed);
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    let token = backend
        .legion_fences
        .export(&host, 7, -1, &[0; 32])
        .expect("export");
    backend.legion_fences.arm(7, token).expect("arm");
    assert!(backend.begin_legion_fence_event(token).is_some());
    assert!(backend.begin_legion_fence_event(token).is_none());
    backend.legion_fences.close_owner(7);
    backend.finish_legion_fence_event(token, true);
    backend.finish_legion_fence_event(token, true);
    assert_eq!(backend.legion_fences.metrics, [1, 1, 1, 0]);
    assert!(backend.legion_fences.live.is_empty());

    let token = backend
        .legion_fences
        .export(&host, 7, -1, &[0; 32])
        .expect("next export");
    backend.legion_fences.arm(7, token).expect("arm");
    assert!(backend.begin_legion_fence_event(token).is_some());
    backend.finish_legion_fence_event(token, false);
    assert_eq!(backend.legion_fences.metrics[2], 1);
    assert!(backend.begin_legion_fence_event(token).is_some());
    backend.legion_fences.close_owner(7);
    backend.finish_legion_fence_event(token, false);
    assert_eq!(backend.legion_fences.metrics[2], 1);
    assert!(backend.begin_legion_fence_event(token).is_none());
}

#[test]
fn checked_combined_dispatch_only_arms_pending_producers_and_retains_all_tokens() {
    for status in [0, 1, -libc::ETIMEDOUT] {
        let host = host();
        host.status.store(status, Ordering::Relaxed);
        let mut backend = NvidiaBackend::for_test();
        backend.host = Box::new(host.clone());
        backend.current_handle = 7;
        backend.legion_fences.enabled = true;
        backend.handle_kinds.insert(7, DeviceKind::Dri(0));
        let mut packet = [0; 32];
        packet[16..24].copy_from_slice(&999_u64.to_le_bytes());
        let mut reply = [0; 128];
        backend.handle_fence_probe(0, -1, EXPORT_ARM_STATUS, &packet, &mut reply);
        assert_eq!(word(&reply, 8), 0);
        let prefix = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        let token = wide(&reply, prefix + 16);
        assert_eq!(word(&reply, prefix + 24) as i32, status);
        assert_eq!(word(&reply, prefix + 28), 0);
        assert_eq!(backend.legion_fences.live[&(7, token)].cookie, 999);
        assert_eq!(backend.legion_fences.added.len(), usize::from(status == 0));
        assert_eq!(
            backend.legion_fences.metrics,
            [1, u64::from(status == 0), 0, 0]
        );
        backend.legion_fences.close_owner(7);
        assert!(backend.legion_fences.live.is_empty() && backend.legion_fences.added.is_empty());
    }
}

#[test]
fn pure_success_without_a_descriptor_is_not_an_export() {
    struct Missing;
    impl HostDriver for Missing {
        fn ioctl(&self, _: RawFd, _: u64, _: &mut [u8]) -> std::result::Result<(), i32> {
            Ok(())
        }
    }
    let mut fences = Fences::default();
    assert_eq!(fences.export(&Missing, 1, -1, &[0; 32]), Err(libc::EIO));
    assert_eq!(fences.next, 1);
    assert!(fences.live.is_empty());
}

#[test]
fn notifications_wait_for_arm_and_commit_and_preserve_producer_error() {
    let host = host();
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    let token = backend
        .legion_fences
        .export(&host, 7, -1, &[0; 32])
        .expect("export");
    assert!(backend.prepare_legion_fence_event(token).is_none());
    assert_eq!(host.calls.load(Ordering::Relaxed), 1);
    assert_eq!(backend.legion_fences.arm(8, token), Err(libc::ENOENT));
    backend.legion_fences.arm(7, token).expect("arm");
    backend.legion_fences.arm(7, token).expect("idempotent arm");
    assert_eq!(backend.legion_fences.added.len(), 1);
    assert!(backend.prepare_legion_fence_event(token).is_none());
    host.status.store(1, Ordering::Relaxed);
    let packet = backend
        .prepare_legion_fence_event(token)
        .expect("ready packet");
    assert_eq!(word(&packet, 4), 7);
    assert_eq!(word(&packet, 8), 1);
    assert_eq!(word(&packet, 12), 0x4c47_0002);
    assert_eq!(wide(&packet, 16), token);
    assert_eq!(backend.prepare_legion_fence_event(token), Some(packet));
    assert!(!backend.legion_fence_watch_finished(token));
    backend.commit_legion_fence_event(token);
    backend.commit_legion_fence_event(token);
    assert!(backend.prepare_legion_fence_event(token).is_none());
    assert!(backend.legion_fence_watch_finished(token));
    assert_eq!(backend.legion_fences.metrics, [1, 1, 1, 0]);
    let second = backend
        .legion_fences
        .export(&host, 7, -1, &[0; 32])
        .expect("second export");
    backend.legion_fences.arm(7, second).expect("second arm");
    host.status.store(-libc::ETIMEDOUT, Ordering::Relaxed);
    assert_eq!(
        word(
            &backend
                .prepare_legion_fence_event(second)
                .expect("error packet"),
            8
        ) as i32,
        -libc::ETIMEDOUT
    );
}

#[test]
fn closing_armed_owner_drops_queued_duplicates_and_stale_notifications() {
    let host = host();
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    let token = backend
        .legion_fences
        .export(&host, 7, -1, &[0; 32])
        .expect("export");
    backend.legion_fences.arm(7, token).expect("arm");
    backend.legion_fences.close_owner(7);
    assert!(backend.legion_fences.added.is_empty());
    assert_eq!(backend.legion_fences.removed, [token]);
    assert_eq!(backend.legion_fences.arm(7, token), Err(libc::ENOENT));
    assert!(backend.prepare_legion_fence_event(token).is_none());
    assert!(backend.legion_fence_watch_finished(token));
    backend.commit_legion_fence_event(token);
    assert_eq!(backend.legion_fences.metrics[2], 0);
    let mut peers = host.peers.lock().expect("peer lock");
    peers[0].set_nonblocking(true).expect("nonblocking");
    assert_eq!(peers[0].read(&mut [0]).expect("all owners closed"), 0);
}

#[test]
fn context_preserves_resource_handles_and_restores_guest_pointer() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut backend = NvidiaBackend::for_test();
    backend.legion_fences.enabled = true;
    backend.host = Box::new(ContextHost(calls.clone()));
    backend.current_data_len = 32;
    let packet = context_packet();
    let mut response = [0; 128];
    let n = backend.handle_fence_context(0, -1, &packet, &mut response);
    let prefix = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    assert_eq!(n, prefix + 48);
    assert_eq!(&response[prefix + 8..prefix + 16], &packet[8..16]);
    assert_eq!(word(&response, prefix + 24), 77);
    assert_eq!(&response[prefix + 32..n], &packet[32..]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn context_rejects_disabled_malformed_and_short_responses_without_host_call() {
    let host = host();
    let mut backend = NvidiaBackend::for_test();
    backend.host = Box::new(host.clone());
    backend.legion_fences.enabled = false;
    let mut response = [0; 128];
    let packet = context_packet();
    backend.handle_fence_context(0, -1, &packet, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::EOPNOTSUPP);
    backend.legion_fences.enabled = true;
    for offset in [16, 28, 40] {
        let mut bad = packet;
        if offset == 40 {
            bad[40..].fill(0);
        } else {
            bad[offset] ^= 1;
        }
        backend.handle_fence_context(0, -1, &bad, &mut response);
        assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    }
    backend.handle_fence_context(0, -1, &packet[..47], &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    backend.handle_fence_context(0, -1, &packet, &mut response[..16]);
    assert_eq!(word(&response, 8) as i32, -libc::ENOSPC);
    assert_eq!(host.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn context_wire_routing_checks_render_file_and_nested_layout() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut backend = NvidiaBackend::for_test();
    backend.legion_fences.enabled = true;
    backend.host = Box::new(ContextHost(calls.clone()));
    let file = std::fs::File::open("/dev/null").expect("mock file");
    let owner = backend.handles.insert(file.into());
    let header = MsgHeader {
        msg_type: MsgType::Ioctl as u32,
        handle: u32::try_from(owner).expect("small handle"),
        status: 0,
        padding: 0,
    };
    let request = IoctlReq {
        cmd: CONTEXT_CREATE as u32,
        data_len: 32,
        nested_offset: 32,
        nested_len: 16,
        deep_ptr_offset: 0,
        deep_len: 0,
    };
    let prefix = size_of::<MsgHeader>() + size_of::<IoctlReq>();
    let mut wire = vec![0; prefix + 48];
    write_struct(&mut wire, &header);
    write_struct(&mut wire[size_of::<MsgHeader>()..], &request);
    wire[prefix..].copy_from_slice(&context_packet());
    let mut response = [0; 128];
    backend.handle_kinds.insert(owner, DeviceKind::Ctl);
    backend.dispatch(&wire, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::ENOTTY);
    backend.handle_kinds.insert(owner, DeviceKind::Dri(0));
    let mut bad = wire.clone();
    bad[size_of::<MsgHeader>() + 8..size_of::<MsgHeader>() + 12].fill(0);
    backend.dispatch(&bad, &mut response);
    assert_eq!(word(&response, 8) as i32, -libc::EINVAL);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    backend.dispatch(&wire, &mut response);
    assert_eq!(word(&response, 8), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn tokens_preserve_pending_success_and_producer_errors_and_scope() {
    let host = host();
    let mut fences = Fences::default();
    let id = fences.export(&host, 7, -1, &[0; 32]).expect("export");
    assert_eq!(fences.status(&host, 7, id), Ok(0));
    host.status.store(1, Ordering::Relaxed);
    assert_eq!(fences.status(&host, 7, id), Ok(1));
    host.status.store(-libc::ETIMEDOUT, Ordering::Relaxed);
    assert_eq!(fences.status(&host, 7, id), Ok(-libc::ETIMEDOUT));
    let before = host.calls.load(Ordering::Relaxed);
    assert_eq!(fences.status(&host, 8, id), Err(libc::ENOENT));
    assert_eq!(host.calls.load(Ordering::Relaxed), before);
    fences.close_owner(7);
    assert_eq!(fences.status(&host, 7, id), Err(libc::ENOENT));
    let new_id = fences.export(&host, 7, -1, &[0; 32]).expect("new export");
    assert_ne!(id, new_id);
    fences.clear();
    let next_id = fences
        .export(&host, 7, -1, &[0; 32])
        .expect("export after reset");
    assert_ne!(new_id, next_id);
}

#[test]
fn bounded_registry_and_counter_refuse_before_host_call() {
    let host = host();
    let mut fences = Fences::default();
    for _ in 0..MAX_FENCES {
        fences
            .export(&host, 1, -1, &[0; 32])
            .expect("bounded export");
    }
    let before = host.calls.load(Ordering::Relaxed);
    assert_eq!(fences.export(&host, 1, -1, &[0; 32]), Err(libc::ENOSPC));
    assert_eq!(host.calls.load(Ordering::Relaxed), before);
    fences.clear();
    fences.next = u64::MAX;
    assert_eq!(fences.export(&host, 1, -1, &[0; 32]), Err(libc::EOVERFLOW));
    assert_eq!(host.calls.load(Ordering::Relaxed), before);
}

#[test]
fn closing_owner_releases_only_its_owned_descriptors() {
    let host = host();
    let mut fences = Fences::default();
    fences.export(&host, 1, -1, &[0; 32]).expect("first export");
    fences
        .export(&host, 2, -1, &[0; 32])
        .expect("second export");
    fences.close_owner(1);
    let mut peers = host.peers.lock().expect("peer lock");
    for peer in peers.iter() {
        peer.set_nonblocking(true).expect("nonblocking");
    }
    assert_eq!(peers[0].read(&mut [0]).expect("closed peer"), 0);
    assert_eq!(
        peers[1].read(&mut [0]).expect_err("still open").kind(),
        std::io::ErrorKind::WouldBlock
    );
}
