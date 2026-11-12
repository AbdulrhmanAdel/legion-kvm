//! Opt-in host presentation.
use super::*;
use legion_display_channel::rpc::{EXCHANGE_BYTES, FRAME_BYTES, Frame, RELEASE_CAPACITY, Releases};
use legion_gpu_presenter::{Format, Image, Plane, Presenter};
use std::os::fd::FromRawFd;

pub(super) struct Display {
    presenter: Presenter,
    client: Option<u32>,
    next: u64,
    notifications: bool,
    quiet_events: bool,
    event_batches: u64,
    event_acks: u64,
    interrupt_requests: u64,
    error: Option<String>,
    pending_event: Option<Releases>,
}

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("validated packet"))
}
fn wide(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("validated packet"))
}

fn export_owned_dmabuf(
    host: &dyn HostDriver,
    file: RawFd,
    gem: u32,
) -> std::result::Result<OwnedFd, i32> {
    let mut prime = [0_u8; 12];
    prime[..4].copy_from_slice(&gem.to_le_bytes());
    let flags = u32::try_from(libc::O_CLOEXEC | libc::O_RDWR).map_err(|_| libc::EINVAL)?;
    prime[4..8].copy_from_slice(&flags.to_le_bytes());
    prime[8..].copy_from_slice(&(-1_i32).to_le_bytes());
    host.ioctl(file, 0xc00c_642d, &mut prime)?;
    let fd = i32::from_le_bytes(prime[8..12].try_into().expect("fixed PRIME packet"));
    if fd < 0 {
        return Err(libc::EIO);
    }
    // SAFETY: successful PRIME_HANDLE_TO_FD returns a new CLOEXEC FD.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn write_releases(presenter: &mut Presenter, out: &mut [u8]) -> anyhow::Result<()> {
    let mut batch = Releases::default();
    while batch.as_slice().len() < RELEASE_CAPACITY {
        let Some(release) = presenter.pop_release() else {
            break;
        };
        batch.push(Frame {
            resource: release.id,
            frame: release.frame,
        })?;
    }
    batch.encode(out)
}

impl NvidiaBackend {
    pub fn enable_legion_display(&mut self, width: i32, height: i32) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.caps.has(crate::caps::GRAPHICS),
            "presentation requires graphics capability"
        );
        self.legion_display = Some(Display {
            presenter: Presenter::connect(width, height)?,
            client: None,
            next: 1,
            notifications: false,
            quiet_events: false,
            event_batches: 0,
            event_acks: 0,
            interrupt_requests: 0,
            error: None,
            pending_event: None,
        });
        Ok(())
    }

    pub fn legion_display_event_fd(&mut self) -> Option<OwnedFd> {
        let display = self.legion_display.as_mut()?;
        if !display.notifications {
            return None;
        }
        match display.presenter.event_fd().try_clone_to_owned() {
            Ok(fd) => Some(fd),
            Err(error) => {
                display.error = Some(format!("display event descriptor: {error}"));
                None
            }
        }
    }

    pub fn prepare_legion_display_event(&mut self) -> Option<(u32, i32, [u8; 264], bool)> {
        let display = self.legion_display.as_mut()?;
        if !display.notifications {
            return None;
        }
        if display.error.is_none()
            && let Err(error) = display.presenter.pump()
        {
            display.error = Some(format!("{error:#}"));
        }
        let owner = display.client?;
        let mut packet = [0; 264];
        if display.error.is_some() {
            return Some((owner, -libc::EIO, packet, !display.quiet_events));
        }
        if display.pending_event.is_none() {
            let mut batch = Releases::default();
            while batch.as_slice().len() < RELEASE_CAPACITY {
                let Some(release) = display.presenter.pop_release() else {
                    break;
                };
                if let Err(error) = batch.push(Frame {
                    resource: release.id,
                    frame: release.frame,
                }) {
                    display.error = Some(format!("{error:#}"));
                    return Some((owner, -libc::EIO, packet, !display.quiet_events));
                }
            }
            if batch.as_slice().is_empty() {
                return None;
            }
            display.pending_event = Some(batch);
        }
        if let Err(error) = display.pending_event.as_ref()?.encode(&mut packet) {
            display.error = Some(format!("{error:#}"));
            return Some((owner, -libc::EIO, [0; 264], !display.quiet_events));
        }
        Some((owner, 0, packet, !display.quiet_events))
    }

    pub fn finish_legion_display_event(&mut self, owner: u32) {
        if let Some(display) = &mut self.legion_display
            && display.client == Some(owner)
        {
            display.event_batches += 1;
            display.event_acks += display
                .pending_event
                .as_ref()
                .map_or(0, |batch| batch.as_slice().len() as u64);
            display.interrupt_requests += u64::from(!display.quiet_events);
            display.pending_event = None;
        }
    }

    pub(super) fn handle_legion_display(
        &mut self,
        host_fd: RawFd,
        cmd: u32,
        data: &[u8],
        response: &mut [u8],
    ) -> usize {
        let outcome = (|| -> anyhow::Result<Vec<u8>> {
            anyhow::ensure!(
                matches!(
                    self.handle_kinds.get(&u64::from(self.current_handle)),
                    Some(DeviceKind::Dri(_))
                ),
                "presentation requires an owned DRM file"
            );
            let nr = cmd & 0xff;
            let size = match nr {
                1 => 4112,
                2 => 48,
                3 => 16,
                4 => 264,
                5 => 8,
                6 => 40,
                7 => EXCHANGE_BYTES,
                9 => 24,
                10 => 16,
                11 => 32,
                12 | 13 => 1024,
                _ => anyhow::bail!("unknown presentation operation"),
            };
            anyhow::ensure!(
                data.len() == size && ((cmd >> 16) & 0x3fff) as usize == size,
                "presentation packet size mismatch"
            );
            anyhow::ensure!(
                cmd >> 30 == 3,
                "presentation ioctl requires read/write direction"
            );
            let mut out = data.to_vec();
            let display = self
                .legion_display
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("host presentation is disabled"))?;
            if !matches!(nr, 3 | 7 | 10) {
                display.presenter.pump()?;
            }
            if nr == 1 {
                anyhow::ensure!(
                    display.client.is_none_or(|id| id == self.current_handle),
                    "presentation belongs to another file"
                );
                display.client = Some(self.current_handle);
                display.notifications = matches!(word(data, 0), 4..=6);
                display.quiet_events = word(data, 0) >= 5;
            } else {
                let client = if nr == 2 {
                    word(data, 40)
                } else {
                    self.current_handle
                };
                anyhow::ensure!(display.client == Some(client), "presentation file mismatch");
            }
            anyhow::ensure!(
                display.error.is_none(),
                "display events failed: {:?}",
                display.error
            );
            match nr {
                1 => {
                    let info = display.presenter.info()?;
                    anyhow::ensure!(
                        info.formats.len() <= 256,
                        "too many display formats for v1 packet"
                    );
                    out.fill(0);
                    out[..4].copy_from_slice(&8_u32.to_le_bytes());
                    out[4..8].copy_from_slice(&u32::try_from(info.formats.len())?.to_le_bytes());
                    out[8..16].copy_from_slice(&info.main_device.to_le_bytes());
                    for (index, format) in info.formats.iter().enumerate() {
                        let at = 16 + index * 16;
                        out[at..at + 4].copy_from_slice(&format.fourcc.to_le_bytes());
                        out[at + 8..at + 16].copy_from_slice(&format.modifier.to_le_bytes());
                    }
                }
                2 => {
                    let fd = export_owned_dmabuf(self.host.as_ref(), host_fd, word(data, 0))
                        .map_err(|errno| anyhow::anyhow!("host PRIME export errno {errno}"))?;
                    let id = display.next;
                    display.next = display
                        .next
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("resource ID overflow"))?;
                    display.presenter.register(Image {
                        id,
                        width: i32::try_from(word(data, 4))?,
                        height: i32::try_from(word(data, 8))?,
                        format: Format {
                            fourcc: word(data, 12),
                            modifier: wide(data, 16),
                        },
                        y_inverted: word(data, 44) & 1 != 0,
                        planes: vec![Plane {
                            fd,
                            stride: word(data, 24),
                            offset: word(data, 28),
                        }],
                    })?;
                    out[32..40].copy_from_slice(&id.to_le_bytes());
                }
                3 => display.presenter.present(wide(data, 0), wide(data, 8))?,
                11 => display.presenter.input()?.encode(&mut out)?,
                12 => display.presenter.input_events(&mut out)?,
                13 => display.presenter.input_keymap(&mut out)?,
                10 => {
                    anyhow::ensure!(
                        display.notifications && display.quiet_events,
                        "queued submission requires active completion polling"
                    );
                    let frame = Frame::decode(data)?;
                    display.presenter.present(frame.resource, frame.frame)?;
                }
                4 => {
                    anyhow::ensure!(
                        !display.notifications,
                        "event-mode completions are guest-local"
                    );
                    write_releases(&mut display.presenter, &mut out)?;
                }
                7 => {
                    let frame = Frame::decode(&data[..FRAME_BYTES])?;
                    display.presenter.present(frame.resource, frame.frame)?;
                    anyhow::ensure!(
                        !display.notifications,
                        "event mode uses ordinary submissions"
                    );
                    write_releases(&mut display.presenter, &mut out[FRAME_BYTES..])?;
                }
                9 => {
                    for (index, value) in [
                        display.event_batches,
                        display.event_acks,
                        display.interrupt_requests,
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
                    }
                }
                5 => display.presenter.retire(wide(data, 0))?,
                6 => {
                    let s = display.presenter.stats();
                    for (index, value) in
                        [s.imports, s.commits, s.releases, s.callbacks, s.fence_waits]
                            .into_iter()
                            .enumerate()
                    {
                        out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
                    }
                }
                _ => unreachable!(),
            }
            Ok(out)
        })();
        match outcome {
            Ok(data) => self.write_ioctl_resp(response, 0, &data),
            Err(error) => {
                if cmd & 0xff == 10
                    && let Some(display) = &mut self.legion_display
                    && display.client == Some(self.current_handle)
                {
                    display.error = Some(format!("queued submission: {error:#}"));
                }
                log::warn!("Legion display operation {} failed: {error:#}", cmd & 0xff);
                self.write_error_resp(response, Status::InvalidMsgType, 0, libc::EIO)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct RefusingKernel;
    impl HostDriver for RefusingKernel {
        fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
            assert_eq!(fd, 17);
            assert_eq!(request, 0xc00c_642d);
            assert_eq!(arg.len(), 12);
            assert_eq!(word(arg, 0), 7);
            let flags = word(arg, 4);
            assert_ne!(flags & u32::try_from(libc::O_CLOEXEC).expect("flag"), 0);
            assert_eq!(
                flags & u32::try_from(libc::O_ACCMODE).expect("mode"),
                u32::try_from(libc::O_RDWR).expect("RDWR")
            );
            assert_eq!(i32::from_le_bytes(arg[8..].try_into().expect("FD")), -1);
            Err(libc::EACCES)
        }
    }
    #[test]
    fn prime_export_uses_real_drm_flags_and_preserves_kernel_error() {
        assert_eq!(
            export_owned_dmabuf(&RefusingKernel, 17, 7).err(),
            Some(libc::EACCES)
        );
    }
    #[test]
    #[cfg(not(miri))] // The backend constructor creates a real SHM memfd.
    fn display_requests_on_non_drm_handles_fail_without_calling_the_kernel() {
        let mut backend = NvidiaBackend::with_default_zones();
        backend.current_handle = 17;
        backend.handle_kinds.insert(17, DeviceKind::Ctl);
        backend.set_host(Box::new(RefusingKernel));
        let mut response = [0_u8; 64];
        let n = backend.handle_legion_display(-1, 0xc030_4c02, &[0; 48], &mut response);
        assert_eq!(n, 16);
        assert!(i32::from_le_bytes(response[8..12].try_into().expect("status")) < 0);
    }
}
