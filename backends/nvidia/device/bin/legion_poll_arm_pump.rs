// One native poll result for each explicit guest wait arm.
fn event_pump(
    rx: Receiver<Watch>,
    vring: VringRwLock,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
    nvidia: Arc<Mutex<NvidiaBackend>>,
    wake: Arc<EventFd>,
) {
    let mut watched = HashMap::<u64, OwnedFd>::new();
    let mut interest = legion_poll_interest::Interests::default();
    let mut pending = PendingEvents::default();
    let mut fence_pending = legion_ready_fences::ReadyFences::default();
    let mut poll_fds = Vec::<libc::pollfd>::new();
    let mut keys = Vec::<u64>::new();
    let mut last_display = Instant::now();
    loop {
        loop {
            match rx.try_recv() {
                Ok(Watch::Display(fd)) => {
                    watched.insert(DISPLAY_EVENT_KEY, fd);
                }
                Ok(Watch::Fence(token, fd)) => {
                    watched.insert(FENCE_EVENT_KEY | token, fd);
                }
                Ok(Watch::RemoveFence(token)) => {
                    let key = FENCE_EVENT_KEY | token;
                    fence_pending.remove(key);
                    watched.remove(&key);
                }
                Ok(Watch::Add(handle, fd)) => {
                    interest.opened(handle);
                    watched.insert(u64::from(handle), fd);
                }
                Ok(Watch::Subscribe(handle)) => {
                    interest.subscribe(handle);
                }
                Ok(Watch::Remove(handle)) => {
                    pending.remove(handle);
                    interest.close(handle);
                    watched.remove(&u64::from(handle));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if last_display.elapsed() >= Duration::from_millis(1) {
            last_display = Instant::now();
            if watched.contains_key(&DISPLAY_EVENT_KEY) {
                display_events(&nvidia, &vring, &mem);
            }
        }
        pending.flush(|handle| push_event(&vring, &mem, handle));
        fence_pending.retry(|key| {
            if fence_ready(&nvidia, &vring, &mem, key) {
                watched.remove(&key);
                true
            } else {
                false
            }
        });
        poll_fds.clear();
        keys.clear();
        keys.push(PUMP_WAKE_KEY);
        poll_fds.push(libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for (&key, fd) in &watched {
            // Ready sync-files remain readable.
            if fence_pending.contains(key)
                || (key != DISPLAY_EVENT_KEY
                    && key & FENCE_EVENT_KEY == 0
                    && !interest.contains(key as u32))
            {
                continue;
            }
            keys.push(key);
            poll_fds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLPRI,
                revents: 0,
            });
        }
        // SAFETY: the array is stable through poll; watched retains every native file and wake retains the eventfd.
        let count = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as libc::nfds_t, 1) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("armed event poll: {error}");
            return;
        }
        for (&key, pfd) in keys.iter().zip(&poll_fds) {
            if pfd.revents & (libc::POLLIN | libc::POLLPRI | libc::POLLERR | libc::POLLHUP) == 0 {
                continue;
            }
            if key == PUMP_WAKE_KEY {
                if let Err(error) = wake.read()
                    && error.kind() != std::io::ErrorKind::WouldBlock
                {
                    log::error!("armed event wake: {error}");
                    return;
                }
            } else if key == DISPLAY_EVENT_KEY {
                display_events(&nvidia, &vring, &mem);
            } else if key & FENCE_EVENT_KEY != 0 {
                fence_pending.mark(key);
            } else {
                // Retain a consumed native result until accepted, but stop polling this file until a guest actually waits on it again.
                interest.disarm(key as u32);
                pending.mark(key as u32);
            }
        }
        pending.flush(|handle| push_event(&vring, &mem, handle));
    }
}
