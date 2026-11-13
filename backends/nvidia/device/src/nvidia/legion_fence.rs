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
#[path = "../../tests/unit/nvidia/legion_fence.rs"]
mod tests;
