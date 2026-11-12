//! Real host fences behind scoped tokens and typed completion notifications.
#![deny(clippy::undocumented_unsafe_blocks, clippy::unwrap_used)]
use super::*;
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;

const CONTEXT_CREATE: u64 = 0xc020_6454;
const FENCE_CREATE: u64 = 0xc018_6455;
const FENCE_WAIT: u64 = 0x4018_6456;
const FILE_INFO: u64 = 0xc038_3e04;
const EXPORT: u32 = 0xc020_4c20;
const STATUS: u32 = 0xc010_4c21;
const CLOSE: u32 = 0x4008_4c22;
const LIVE: u32 = 0x8008_4c23;
const ARM: u32 = 0x4008_4c24;
const LIVE_ALL: u32 = 0x8008_4c25;
const METRICS: u32 = 0x8020_4c26;
const WAIT: u32 = 0x4020_4c27;
const WAIT_MANY: u32 = 0x4120_4c28;
const ATTACH: u32 = 0x4018_4c29;
const WAIT_DONE: u32 = 0x4018_4c2a;
const EXPORT_STATUS: u32 = 0xc020_4c2b;
const EXPORT_ARM: u32 = 0xc020_4c2c;
const EXPORT_ARM_STATUS: u32 = 0xc020_4c2d;
const PRIME_EXPORT: u64 = 0xc00c_642d;
const IMPORT_SYNC_FILE: u64 = 0x4008_6203;
const SYNC_MERGE: u64 = 0xc030_3e03;
const MAX_WAIT_INPUTS: usize = 32;
const WAIT_MANY_BYTES: usize = 32 + MAX_WAIT_INPUTS * 8;
const MAX_FENCES: usize = 256;
const RM_CONTROL: u64 = 0xc020_462a;
const HANDLE_INFO: u32 = 0x0d02;
const CHILD_HANDLE: u32 = 0x0d05;
const GPU_ID: u32 = 0x2080_0142;
const SURFACE_LAYOUT: u32 = 0x2080_1352;

#[derive(Clone, Copy)]
struct SurfaceLayout {
    stride: u64,
    maximum: u64,
    monitored: u64,
    caps: u32,
}

impl SurfaceLayout {
    fn validate(self, index: u64, bytes: u64) -> std::result::Result<(), i32> {
        if self.stride < 8 || !self.stride.is_multiple_of(8) || self.caps & !3 != 0 {
            return Err(libc::EOPNOTSUPP);
        }
        for offset in [
            Some(self.maximum),
            (self.caps & 1 != 0).then_some(self.monitored),
        ]
        .into_iter()
        .flatten()
        {
            if !offset.is_multiple_of(8)
                || offset.checked_add(8).is_none_or(|end| end > self.stride)
            {
                return Err(libc::EINVAL);
            }
        }
        if index
            .checked_add(1)
            .and_then(|slots| slots.checked_mul(self.stride))
            .is_none_or(|end| end > bytes)
        {
            return Err(libc::EINVAL);
        }
        Ok(())
    }
}

fn native_control(
    host: &dyn HostDriver,
    file: RawFd,
    client: u32,
    object: u32,
    command: u32,
    data: &mut [u8],
) -> std::result::Result<(), i32> {
    let size = u32::try_from(data.len()).map_err(|_| libc::EOVERFLOW)?;
    let mut outer = [0_u8; 32];
    outer[..4].copy_from_slice(&client.to_le_bytes());
    outer[4..8].copy_from_slice(&object.to_le_bytes());
    outer[8..12].copy_from_slice(&command.to_le_bytes());
    outer[16..24].copy_from_slice(&(data.as_mut_ptr() as u64).to_le_bytes());
    outer[24..28].copy_from_slice(&size.to_le_bytes());
    outer[28..].fill(0xff);
    // The pointed-to buffer is owned and live through the synchronous ioctl.
    host.ioctl(file, RM_CONTROL, &mut outer)?;
    if word(&outer, 28) != 0 {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn handle_info(
    host: &dyn HostDriver,
    file: RawFd,
    client: u32,
    object: u32,
    index: u32,
) -> std::result::Result<u32, i32> {
    let mut data = [0_u8; 16];
    data[..4].copy_from_slice(&object.to_le_bytes());
    data[4..8].copy_from_slice(&index.to_le_bytes());
    native_control(host, file, client, client, HANDLE_INFO, &mut data)?;
    Ok(word(&data, 8))
}

struct Exported {
    fd: OwnedFd,
    armed: bool,
    notified: bool,
    cookie: u64,
}

pub(super) struct Fences {
    enabled: bool,
    next: u64,
    live: BTreeMap<(u32, u64), Exported>,
    added: Vec<(u64, OwnedFd)>,
    removed: Vec<u64>,
    metrics: [u64; 4], // exports, arms, notifications, guest status queries
    inflight_event: Option<u64>, // single event pump's prepared publication
}

impl Default for Fences {
    fn default() -> Self {
        Self {
            enabled: std::env::var_os("LEGION_FENCE_PROBE").is_some_and(|v| v == "1"),
            next: 1,
            live: BTreeMap::new(),
            added: Vec::new(),
            removed: Vec::new(),
            metrics: [0; 4],
            inflight_event: None,
        }
    }
}

fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().expect("checked packet"))
}

fn wide(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("checked packet"))
}

fn validate_export(input: &[u8]) -> std::result::Result<(), i32> {
    if input.len() != 32 || wide(input, 16) != 0 || word(input, 24) != 0 || word(input, 28) != 0 {
        Err(libc::EINVAL)
    } else {
        Ok(())
    }
}

fn validate_export_status(status: i32) -> std::result::Result<i32, i32> {
    // Linux sync-file aggregate status: pending, success, or -MAX_ERRNO..-1.
    if (-4095..=1).contains(&status) {
        Ok(status)
    } else {
        Err(libc::EPROTO)
    }
}

fn validate_export_arm(input: &[u8]) -> std::result::Result<u64, i32> {
    if input.len() != 32 || wide(input, 24) != 0 {
        return Err(libc::EINVAL);
    }
    let cookie = wide(input, 16);
    if cookie == 0 || cookie >= (1_u64 << 63) {
        Err(libc::EINVAL)
    } else {
        Ok(cookie)
    }
}

fn validate_context(input: &[u8], output_bytes: usize) -> std::result::Result<(), i32> {
    // Exact 615.78.08 ABI.
    if input.len() != 48 || wide(input, 16) != 16 || word(input, 28) != 0 || wide(input, 40) < 8 {
        return Err(libc::EINVAL);
    }
    if output_bytes < size_of::<MsgHeader>() + size_of::<IoctlResp>() + 48 {
        return Err(libc::ENOSPC);
    }
    Ok(())
}

fn validate_wait_many(input: &[u8]) -> std::result::Result<usize, i32> {
    if input.len() != WAIT_MANY_BYTES {
        return Err(libc::EINVAL);
    }
    let count = word(input, 4) as usize;
    if !(1..=MAX_WAIT_INPUTS).contains(&count)
        || wide(input, 8) >= wide(input, 16)
        || wide(input, 24) != 0
        || input[32 + count * 8..].iter().any(|byte| *byte != 0)
    {
        return Err(libc::EINVAL);
    }
    Ok(count)
}

fn validate_attach(input: &[u8]) -> std::result::Result<(), i32> {
    if input.len() != 24 || word(input, 0) == 0 || word(input, 4) > 1 || wide(input, 16) != 0 {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn validate_completed_wait(input: &[u8]) -> std::result::Result<(), i32> {
    if input.len() != 24
        || word(input, 0) == 0
        || word(input, 4) != 0
        || wide(input, 8) >= wide(input, 16)
    {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn completed_wait(
    host: &dyn HostDriver,
    file: RawFd,
    input: &[u8],
) -> std::result::Result<(), i32> {
    validate_completed_wait(input)?;
    let mut create = [0_u8; 24];
    create[..4].copy_from_slice(&input[..4]); // Destination context, point zero.
    create[16..20].copy_from_slice(&(-1_i32).to_le_bytes());
    host.ioctl(file, FENCE_CREATE, &mut create)?;
    let raw = word(&create, 16) as i32;
    if raw < 0 {
        return Err(libc::EIO);
    }
    // SAFETY: FENCE_CREATE returned one newly owned native sync-file.
    let ready = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut info = [0_u8; 56];
    host.ioctl(ready.as_raw_fd(), FILE_INFO, &mut info)?;
    if word(&info, 32) != 1 {
        return Err(libc::EIO);
    }
    let mut wait = [0_u8; 24];
    wait[..4].copy_from_slice(&input[..4]);
    wait[4..8].copy_from_slice(&ready.as_raw_fd().to_le_bytes());
    wait[8..].copy_from_slice(&input[8..]);
    // A completed dependency does not bypass the destination pre-value.
    host.ioctl(file, FENCE_WAIT, &mut wait)
}

fn export_buffer(
    host: &dyn HostDriver,
    file: RawFd,
    handle: u32,
) -> std::result::Result<OwnedFd, i32> {
    let mut params = [0_u8; 12];
    params[..4].copy_from_slice(&handle.to_le_bytes());
    params[4..8].copy_from_slice(&((libc::O_CLOEXEC | libc::O_RDWR) as u32).to_le_bytes());
    params[8..].copy_from_slice(&(-1_i32).to_le_bytes());
    host.ioctl(file, PRIME_EXPORT, &mut params)?;
    let raw = word(&params, 8) as i32;
    if raw < 0 {
        return Err(libc::EIO);
    }
    // SAFETY: successful PRIME_HANDLE_TO_FD creates one owned DMA-BUF descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn merge_native(
    host: &dyn HostDriver,
    left: RawFd,
    right: RawFd,
) -> std::result::Result<OwnedFd, i32> {
    let mut params = [0_u8; 48];
    params[..12].copy_from_slice(b"legion-input");
    params[32..36].copy_from_slice(&right.to_le_bytes());
    params[36..40].copy_from_slice(&(-1_i32).to_le_bytes());
    host.ioctl(left, SYNC_MERGE, &mut params)?;
    let raw = word(&params, 36) as i32;
    if raw < 0 {
        return Err(libc::EIO);
    }
    // SAFETY: successful SYNC_IOC_MERGE returns a newly owned sync-file descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

impl Fences {
    fn attach(
        &self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<(), i32> {
        validate_attach(input)?;
        let fence = self
            .live
            .get(&(owner, wide(input, 8)))
            .ok_or(libc::ENOENT)?;
        let buffer = export_buffer(host, file, word(input, 0))?;
        let mut params = [0_u8; 8];
        let flags: u32 = if word(input, 4) == 1 { 1 } else { 2 }; // DMA_BUF_SYNC_READ / WRITE
        params[..4].copy_from_slice(&flags.to_le_bytes());
        params[4..].copy_from_slice(&fence.fd.as_raw_fd().to_le_bytes());
        // Attach this exact native fence, not a second producer at a similar point/timeout.
        host.ioctl(buffer.as_raw_fd(), IMPORT_SYNC_FILE, &mut params)
    }
    pub(super) fn close_owner(&mut self, owner: u32) {
        let ids: Vec<_> = self
            .live
            .keys()
            .filter(|(file, _)| *file == owner)
            .copied()
            .collect();
        for key in ids {
            self.close(key);
        }
    }

    pub(super) fn clear(&mut self) {
        if self.enabled {
            log::info!(
                "legion fence probe shutdown: exports={} arms={} notifications={} guest_status_queries={} remaining={}",
                self.metrics[0],
                self.metrics[1],
                self.metrics[2],
                self.metrics[3],
                self.live.len()
            );
        }
        self.removed
            .extend(self.live.keys().map(|(_, token)| *token));
        self.live.clear();
        self.added.clear();
        // Never restart token numbering: old tokens must not alias new fences.
    }

    fn export(
        &mut self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<u64, i32> {
        if self.live.len() >= MAX_FENCES {
            return Err(libc::ENOSPC);
        }
        let token = self.next;
        let next = token
            .checked_add(1)
            .filter(|id| *id < (1_u64 << 63))
            .ok_or(libc::EOVERFLOW)?;
        let mut payload = [0_u8; 24];
        payload[..16].copy_from_slice(&input[..16]);
        payload[16..20].copy_from_slice(&(-1_i32).to_le_bytes());
        host.ioctl(file, FENCE_CREATE, &mut payload)?;
        let raw = i32::from_le_bytes(payload[16..20].try_into().expect("fixed fence packet"));
        if raw < 0 {
            return Err(libc::EIO);
        }
        // SAFETY: successful FENCE_CREATE returns one newly owned sync-file descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        self.next = next;
        self.live.insert(
            (owner, token),
            Exported {
                fd,
                armed: false,
                notified: false,
                cookie: 0,
            },
        );
        self.metrics[0] += 1;
        Ok(token)
    }

    fn status(
        &self,
        host: &dyn HostDriver,
        owner: u32,
        token: u64,
    ) -> std::result::Result<i32, i32> {
        let fd = self.live.get(&(owner, token)).ok_or(libc::ENOENT)?;
        // num_fences=0 and a null detailed-info pointer request aggregate status only.
        let mut info = [0_u8; 56];
        host.ioctl(fd.fd.as_raw_fd(), FILE_INFO, &mut info)?;
        Ok(i32::from_le_bytes(
            info[32..36].try_into().expect("fixed info packet"),
        ))
    }

    /// Return a verified native status with the export reply.
    fn export_status(
        &mut self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<(u64, i32), i32> {
        let token = self.export(host, owner, file, input)?;
        match self
            .status(host, owner, token)
            .and_then(validate_export_status)
        {
            Ok(status) => Ok((token, status)),
            Err(errno) => {
                // The guest has not received this token, so rollback owns its only reference.
                self.close((owner, token));
                Err(errno)
            }
        }
    }

    /// The guest installs its proxy before this request, using a correlation cookie.
    fn export_arm(
        &mut self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<u64, i32> {
        self.export_correlated(host, owner, file, input, false)
            .map(|(token, _)| token)
    }

    fn export_arm_status(
        &mut self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<(u64, i32), i32> {
        self.export_correlated(host, owner, file, input, true)
    }

    fn export_correlated(
        &mut self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
        checked: bool,
    ) -> std::result::Result<(u64, i32), i32> {
        let cookie = validate_export_arm(input)?;
        if self
            .live
            .iter()
            .any(|((file, _), entry)| *file == owner && entry.cookie == cookie)
        {
            return Err(libc::EEXIST);
        }
        let (token, status) = if checked {
            self.export_status(host, owner, file, input)?
        } else {
            (self.export(host, owner, file, input)?, 0)
        };
        self.live
            .get_mut(&(owner, token))
            .expect("new export")
            .cookie = cookie;
        if status == 0
            && let Err(errno) = self.arm(owner, token)
        {
            self.close((owner, token));
            return Err(errno);
        }
        Ok((token, status))
    }

    fn wait(
        &self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<(), i32> {
        // Private wire: context, reserved, token, pre-value, post-value.
        if input.len() != 32 || word(input, 4) != 0 || wide(input, 16) >= wide(input, 24) {
            return Err(libc::EINVAL);
        }
        let exported = self
            .live
            .get(&(owner, wide(input, 8)))
            .ok_or(libc::ENOENT)?;
        let mut params = [0_u8; 24];
        params[..4].copy_from_slice(&input[..4]);
        params[4..8].copy_from_slice(&exported.fd.as_raw_fd().to_le_bytes());
        params[8..].copy_from_slice(&input[16..]);
        // The registry keeps the descriptor alive through this synchronous ioctl.
        host.ioctl(file, FENCE_WAIT, &mut params)
    }

    fn close(&mut self, key: (u32, u64)) -> Option<()> {
        self.live.remove(&key)?;
        self.added.retain(|(token, _)| *token != key.1);
        self.removed.push(key.1);
        Some(())
    }

    fn wait_many(
        &self,
        host: &dyn HostDriver,
        owner: u32,
        file: RawFd,
        input: &[u8],
    ) -> std::result::Result<(), i32> {
        let count = validate_wait_many(input)?;
        // Resolve every dependency before invoking any native merge or wait.
        let entries: Vec<_> = (0..count)
            .map(|i| {
                self.live
                    .get(&(owner, wide(input, 32 + i * 8)))
                    .ok_or(libc::ENOENT)
            })
            .collect::<std::result::Result<_, _>>()?;
        let first = entries[0].fd.as_raw_fd();
        let mut merged: Option<OwnedFd> = None;
        for entry in &entries[1..] {
            let left = merged.as_ref().map_or(first, AsRawFd::as_raw_fd);
            merged = Some(merge_native(host, left, entry.fd.as_raw_fd())?);
        }
        let mut params = [0_u8; 24];
        params[..4].copy_from_slice(&input[..4]);
        params[4..8].copy_from_slice(
            &merged
                .as_ref()
                .map_or(first, AsRawFd::as_raw_fd)
                .to_le_bytes(),
        );
        params[8..].copy_from_slice(&input[8..24]);
        // NVIDIA retains the merged dma-fence for its asynchronous wait.
        host.ioctl(file, FENCE_WAIT, &mut params)
    }

    fn arm(&mut self, owner: u32, token: u64) -> std::result::Result<(), i32> {
        let entry = self.live.get_mut(&(owner, token)).ok_or(libc::ENOENT)?;
        if !entry.armed {
            let fd = entry
                .fd
                .try_clone()
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            self.added.push((token, fd));
            entry.armed = true;
            self.metrics[1] += 1;
        }
        Ok(())
    }
}

impl NvidiaBackend {
    fn surface_layout(
        &self,
        render: RawFd,
        input: &[u8],
    ) -> std::result::Result<SurfaceLayout, i32> {
        let client = word(input, 32);
        let surface = word(input, 36);
        // RM clients are associated with particular control files.
        let mut control = None;
        for (handle, kind) in &self.handle_kinds {
            if !matches!(kind, DeviceKind::Ctl) {
                continue;
            }
            let Ok(file) = self.handles.get_raw(*handle) else {
                continue;
            };
            if let Ok(class) = handle_info(&*self.host, file, client, surface, 2) {
                if class != 0xda {
                    return Err(libc::EINVAL);
                }
                control = Some(file);
                break;
            }
        }
        let control = control.ok_or(libc::EOPNOTSUPP)?;
        let mut parent = handle_info(&*self.host, control, client, surface, 1)?;
        let mut class = handle_info(&*self.host, control, client, parent, 2)?;
        if class == 0x80 {
            let mut child = [0_u8; 12];
            child[..4].copy_from_slice(&parent.to_le_bytes());
            child[4..8].copy_from_slice(&0x2080_u32.to_le_bytes());
            native_control(
                &*self.host,
                control,
                client,
                client,
                CHILD_HANDLE,
                &mut child,
            )?;
            parent = word(&child, 8);
            class = handle_info(&*self.host, control, client, parent, 2)?;
        }
        if class != 0x2080 {
            return Err(libc::EOPNOTSUPP);
        }
        let mut gpu = [0_u8; 4];
        native_control(&*self.host, control, client, parent, GPU_ID, &mut gpu)?;
        let mut device = [0_u8; 36];
        self.host.ioctl(render, 0xc024_6443, &mut device)?;
        if word(&gpu, 0) != word(&device, 0) {
            return Err(libc::EXDEV);
        }
        let mut layout = [0_u8; 32];
        native_control(
            &*self.host,
            control,
            client,
            parent,
            SURFACE_LAYOUT,
            &mut layout,
        )?;
        Ok(SurfaceLayout {
            maximum: wide(&layout, 0),
            monitored: wide(&layout, 8),
            stride: wide(&layout, 16),
            caps: word(&layout, 24),
        })
    }

    pub fn take_legion_fence_watches(&mut self) -> (Vec<(u64, OwnedFd)>, Vec<u64>) {
        (
            std::mem::take(&mut self.legion_fences.added),
            std::mem::take(&mut self.legion_fences.removed),
        )
    }

    /// Called only after a real sync-file readiness indication.
    pub fn prepare_legion_fence_event(&self, token: u64) -> Option<[u8; 32]> {
        let (&(owner, _), entry) = self
            .legion_fences
            .live
            .iter()
            .find(|((_, id), _)| *id == token)?;
        if !entry.armed || entry.notified {
            return None;
        }
        let status = self
            .legion_fences
            .status(&*self.host, owner, token)
            .unwrap_or_else(|errno| -errno.abs());
        if status == 0 {
            return None;
        }
        let mut packet = [0; 32];
        packet[..4].copy_from_slice(&(MsgType::EventReady as u32).to_le_bytes());
        packet[4..8].copy_from_slice(&owner.to_le_bytes());
        packet[8..12].copy_from_slice(&status.to_le_bytes());
        packet[12..16].copy_from_slice(&0x4c47_0002_u32.to_le_bytes());
        packet[16..24].copy_from_slice(&token.to_le_bytes());
        packet[24..32].copy_from_slice(&entry.cookie.to_le_bytes());
        Some(packet)
    }

    pub fn commit_legion_fence_event(&mut self, token: u64) {
        if let Some(entry) = self
            .legion_fences
            .live
            .iter_mut()
            .find(|((_, id), _)| *id == token)
            .map(|(_, e)| e)
            .filter(|entry| !entry.notified)
        {
            entry.notified = true;
            self.legion_fences.metrics[2] += 1;
        }
    }

    /// Reserve the single event pump's publication while its queue operation runs without the backend lock.
    pub fn begin_legion_fence_event(&mut self, token: u64) -> Option<[u8; 32]> {
        if self.legion_fences.inflight_event.is_some() {
            return None;
        }
        let packet = self.prepare_legion_fence_event(token)?;
        self.legion_fences.inflight_event = Some(token);
        Some(packet)
    }

    pub fn finish_legion_fence_event(&mut self, token: u64, accepted: bool) {
        if self.legion_fences.inflight_event != Some(token) {
            return;
        }
        self.legion_fences.inflight_event = None;
        if !accepted {
            return;
        }
        if self.legion_fences.live.keys().any(|(_, id)| *id == token) {
            self.commit_legion_fence_event(token);
        } else {
            // The event really reached the guest before CLOSE retired its token.
            self.legion_fences.metrics[2] += 1;
        }
    }

    pub fn legion_fence_watch_finished(&self, token: u64) -> bool {
        self.legion_fences
            .live
            .iter()
            .find(|((_, id), _)| *id == token)
            .is_none_or(|(_, e)| e.notified)
    }

    pub(super) fn handle_fence_context(
        &mut self,
        cookie: u64,
        file: RawFd,
        input: &[u8],
        output: &mut [u8],
    ) -> usize {
        let result = (|| {
            if !self.legion_fences.enabled {
                return Err(libc::EOPNOTSUPP);
            }
            // Exact 615.78.08 ABI: 32-byte outer, 16-byte private block.
            validate_context(input, output.len())?;
            if wide(input, 0) != 0 {
                self.surface_layout(file, input)?
                    .validate(wide(input, 0), wide(input, 40))?;
            }
            let mut outer = input[..32].to_vec();
            let mut nested: [u8; 16] = input[32..].try_into().expect("validated context packet");
            // This private block contains RM handles, NOT a memFd.
            outer[8..16].copy_from_slice(&(nested.as_mut_ptr() as u64).to_le_bytes());
            self.host.ioctl(file, CONTEXT_CREATE, &mut outer)?;
            outer[8..16].copy_from_slice(&input[8..16]);
            outer.extend_from_slice(&nested);
            Ok(outer)
        })();
        match result {
            Ok(bytes) => self.write_ioctl_resp(output, cookie, &bytes),
            Err(errno) => self.write_error_resp(output, Status::IoctlFailed, cookie, errno),
        }
    }

    pub(super) fn handle_fence_probe(
        &mut self,
        cookie: u64,
        file: RawFd,
        command: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> usize {
        let result = (|| {
            if !self.legion_fences.enabled {
                return Err(libc::EOPNOTSUPP);
            }
            let size = match command {
                EXPORT | EXPORT_STATUS | EXPORT_ARM | EXPORT_ARM_STATUS => 32,
                STATUS => 16,
                CLOSE | LIVE | ARM | LIVE_ALL => 8,
                METRICS | WAIT => 32,
                WAIT_MANY => WAIT_MANY_BYTES,
                ATTACH => 24,
                WAIT_DONE => 24,
                _ => return Err(libc::ENOTTY),
            };
            if input.len() != size
                || output.len() < size_of::<MsgHeader>() + size_of::<IoctlResp>() + size
            {
                return Err(libc::EINVAL);
            }
            let owner = self.current_handle;
            if !matches!(
                self.handle_kinds.get(&u64::from(owner)),
                Some(DeviceKind::Dri(_))
            ) {
                return Err(libc::ENOTTY);
            }
            let mut bytes = input.to_vec();
            match command {
                EXPORT_ARM | EXPORT_ARM_STATUS => {
                    let (token, status) = if command == EXPORT_ARM_STATUS {
                        self.legion_fences
                            .export_arm_status(&*self.host, owner, file, input)?
                    } else {
                        (
                            self.legion_fences
                                .export_arm(&*self.host, owner, file, input)?,
                            0,
                        )
                    };
                    bytes[16..24].copy_from_slice(&token.to_le_bytes());
                    bytes[24..28].copy_from_slice(&status.to_le_bytes());
                }
                EXPORT | EXPORT_STATUS => {
                    validate_export(input)?;
                    let (token, status) = if command == EXPORT_STATUS {
                        self.legion_fences
                            .export_status(&*self.host, owner, file, input)?
                    } else {
                        (
                            self.legion_fences.export(&*self.host, owner, file, input)?,
                            0,
                        )
                    };
                    bytes[16..24].copy_from_slice(&token.to_le_bytes());
                    bytes[24..28].copy_from_slice(&status.to_le_bytes());
                }
                STATUS => {
                    if wide(input, 8) != 0 {
                        return Err(libc::EINVAL);
                    }
                    let status = self
                        .legion_fences
                        .status(&*self.host, owner, wide(input, 0))?;
                    self.legion_fences.metrics[3] += 1;
                    bytes[8..12].copy_from_slice(&status.to_le_bytes());
                }
                CLOSE => {
                    self.legion_fences
                        .close((owner, wide(input, 0)))
                        .ok_or(libc::ENOENT)?;
                }
                LIVE => {
                    let count = self
                        .legion_fences
                        .live
                        .keys()
                        .filter(|(file, _)| *file == owner)
                        .count();
                    bytes.copy_from_slice(
                        &u64::try_from(count)
                            .map_err(|_| libc::EOVERFLOW)?
                            .to_le_bytes(),
                    );
                }
                ARM => self.legion_fences.arm(owner, wide(input, 0))?,
                WAIT => self.legion_fences.wait(&*self.host, owner, file, input)?,
                WAIT_MANY => self
                    .legion_fences
                    .wait_many(&*self.host, owner, file, input)?,
                ATTACH => self.legion_fences.attach(&*self.host, owner, file, input)?,
                WAIT_DONE => completed_wait(&*self.host, file, input)?,
                LIVE_ALL => bytes.copy_from_slice(
                    &u64::try_from(self.legion_fences.live.len())
                        .map_err(|_| libc::EOVERFLOW)?
                        .to_le_bytes(),
                ),
                METRICS => {
                    for (index, value) in self.legion_fences.metrics.iter().enumerate() {
                        bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
                    }
                }
                _ => unreachable!("validated probe command"),
            }
            Ok(bytes)
        })();
        match result {
            Ok(bytes) => self.write_ioctl_resp(output, cookie, &bytes),
            Err(errno) => self.write_error_resp(output, Status::IoctlFailed, cookie, errno),
        }
    }
}

#[cfg(test)]
mod tests {
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
            assert!(
                backend.legion_fences.live.is_empty() && backend.legion_fences.added.is_empty()
            );
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
}
