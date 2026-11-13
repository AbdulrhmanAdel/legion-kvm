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
            event_wake: Arc::new(EventFd::new(EFD_CLOEXEC | EFD_NONBLOCK).expect("test wake fd")),
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
