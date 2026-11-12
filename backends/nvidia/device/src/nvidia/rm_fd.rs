//! RM escapes that carry a descriptor or a mapping: REGISTER_FD, OS events, ALLOC_MEMORY, MAP_MEMORY, UNMAP_MEMORY, UPDATE_DEVICE_MAPPING_INFO.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------ FD-carrying ioctl ------------------------------------------------------------------

    pub(super) fn dispatch_fd_carrying(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        escape: u32,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        use abi::ioctl::*;

        let fd_offset: usize = match escape {
            // nv_ioctl_register_fd_t: ctl_fd is the only field, offset 0.
            NV_ESC_REGISTER_FD => 0,
            // nv_ioctl_alloc_os_event_t: hClient(4) + hDevice(4) + fd @ offset 8
            NV_ESC_ALLOC_OS_EVENT => 8,
            // nv_ioctl_free_os_event_t: same layout as alloc, fd @ offset 8
            NV_ESC_FREE_OS_EVENT => 8,
            // NV_ESC_RM_ALLOC_MEMORY: fd at offset 48
            NV_ESC_RM_ALLOC_MEMORY => 48,
            _ => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTTY),
        };

        if param_in.len() < fd_offset + 4 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let embedded = {
            let mut b = [0u8; 4];
            b.copy_from_slice(&param_in[fd_offset..fd_offset + 4]);
            i32::from_le_bytes(b)
        };

        // The field is a descriptor only when the caller put one there. -1 is the caller saying it has none, which several of these ioctls allow -- NV_ESC_RM_ALLOC_MEMORY carries it for every allocation not being made on another open file.
        if embedded < 0 {
            let mut param_buf = param_in.to_vec();
            if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
                log::warn!("ioctl(0x{request:x}) with no embedded fd failed: errno={errno}");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }
            return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
        }

        let guest_embedded = embedded as u64;
        let host_embedded = match self.handles.get_raw(guest_embedded) {
            Ok(fd) => fd,
            Err(_) => {
                log::warn!("fd-carrying ioctl: bad embedded handle {}", guest_embedded);
                return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
            }
        };

        let mut param_buf = param_in.to_vec();
        param_buf[fd_offset..fd_offset + 4].copy_from_slice(&(host_embedded as i32).to_le_bytes());

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
            log::warn!("fd-carrying ioctl(0x{:x}) failed: errno={}", request, errno);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // Restore guest handle in response so user-mode code reading it back gets what it originally wrote.
        param_buf[fd_offset..fd_offset + 4].copy_from_slice(&(guest_embedded as i32).to_le_bytes());

        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    pub(super) fn dispatch_update_device_mapping_info(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        log::debug!(
            "UPDATE_DEVICE_MAPPING_INFO: ENTERED, host_fd={}, param_in.len={}",
            host_fd,
            param_in.len()
        );

        if param_in.len() < 40 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let h_client = u32::from_le_bytes(param_in[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
        let old_cpu_addr = u64::from_le_bytes(param_in[16..24].try_into().unwrap());
        let new_cpu_addr = u64::from_le_bytes(param_in[24..32].try_into().unwrap());

        log::debug!(
            "UPDATE_DEVICE_MAPPING_INFO: client={:#x} mem={:#x} old={:#x} new={:#x}",
            h_client,
            h_memory,
            old_cpu_addr,
            new_cpu_addr
        );

        // The guest sends SHM offsets or guest VAs.
        let mapping_offset = self
            .active_maps
            .update_offset(h_client, h_memory, old_cpu_addr);
        let mut host_old = old_cpu_addr;
        if let Some(entry) =
            mapping_offset.and_then(|offset| self.active_maps.find_by_offset(offset))
        {
            host_old = entry.host_p_linear_address;
            log::debug!(
                "UPDATE_DEVICE_MAPPING_INFO: translated old {:#x} → host {:#x}",
                old_cpu_addr,
                host_old
            );
        }

        let mut param_buf = param_in.to_vec();
        // Set pOldCpuAddress to host VA
        param_buf[16..24].copy_from_slice(&host_old.to_le_bytes());
        // Set pNewCpuAddress to host VA too (the host mapping didn't move)
        param_buf[24..32].copy_from_slice(&host_old.to_le_bytes());

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
            log::warn!(
                "UPDATE_DEVICE_MAPPING_INFO: host ioctl failed: errno={}",
                errno
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        let status = u32::from_le_bytes(param_buf[32..36].try_into().unwrap());
        log::debug!("UPDATE_DEVICE_MAPPING_INFO: host status=0x{:x}", status);
        if status == 0
            && let Some(offset) = mapping_offset
        {
            self.active_maps.note_guest_address(offset, new_cpu_addr);
        }

        // NVIDIA's escape handler only updates status.
        param_buf[16..32].copy_from_slice(&param_in[16..32]);

        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    pub(super) fn dispatch_map_memory(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        use crate::shm::PgprotKind;

        const _NVOS33_SIZE: usize = 48;
        const WITH_FD_SIZE: usize = 56;
        const FD_OFFSET: usize = 48;
        const LENGTH_OFFSET: usize = 24;
        const STATUS_OFFSET: usize = 40;
        const FLAGS_OFFSET: usize = 44;

        const FLAGS_CACHING_TYPE_SHIFT: u32 = 23;
        const FLAGS_CACHING_TYPE_MASK: u32 = 0x7;
        const CACHING_TYPE_CACHED: u32 = 0;
        const CACHING_TYPE_UNCACHED: u32 = 1;
        const CACHING_TYPE_WRITECOMBINED: u32 = 2;
        const CACHING_TYPE_WRITEBACK: u32 = 5;
        const CACHING_TYPE_DEFAULT: u32 = 6;
        const CACHING_TYPE_UNCACHED_WEAK: u32 = 7;

        if param_in.len() < WITH_FD_SIZE {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        // --- Step 1: Translate embedded FD (guest handle → host fd) ---

        let guest_fd_handle = {
            let mut b = [0u8; 4];
            b.copy_from_slice(&param_in[FD_OFFSET..FD_OFFSET + 4]);
            i32::from_le_bytes(b) as u64
        };

        let host_map_fd = match self.handles.get_raw(guest_fd_handle) {
            Ok(fd) => fd,
            Err(_) => {
                log::warn!(
                    "NV_ESC_RM_MAP_MEMORY: bad embedded FD handle {}",
                    guest_fd_handle
                );
                return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
            }
        };

        // Refuse before arming a host mapping that the guest cannot use.
        let Some(window) = self.window.as_ref() else {
            log::warn!(
                "NV_ESC_RM_MAP_MEMORY: no shared window, so device memory cannot be \
                 addressed by the guest"
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTSUP);
        };

        // A host fd is single-use for mapping.
        if self.active_maps.fd_has_mapping(guest_fd_handle) {
            log::warn!(
                "NV_ESC_RM_MAP_MEMORY: handle {guest_fd_handle} already carries a mapping; \
                 a host fd cannot carry two, so the host will return NV_ERR_STATE_IN_USE"
            );
        }

        let mut param_buf = param_in.to_vec();
        param_buf[FD_OFFSET..FD_OFFSET + 4].copy_from_slice(&(host_map_fd as i32).to_le_bytes());

        // --- Step 2: Call host ioctl ---

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
            log::warn!("NV_ESC_RM_MAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore guest handle before returning
            param_buf[FD_OFFSET..FD_OFFSET + 4]
                .copy_from_slice(&(guest_fd_handle as i32).to_le_bytes());
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // --- Step 3: Check RM status and read updated fields ---

        let rm_status = u32::from_le_bytes(
            param_buf[STATUS_OFFSET..STATUS_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        // Restore guest handle in param_buf for copy-out regardless of status.
        param_buf[FD_OFFSET..FD_OFFSET + 4]
            .copy_from_slice(&(guest_fd_handle as i32).to_le_bytes());

        if rm_status != 0 {
            // RM returned an error status (NV_OK == 0).
            log::debug!("NV_ESC_RM_MAP_MEMORY: RM status 0x{:x}", rm_status);
            return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
        }

        let length = u64::from_le_bytes(
            param_buf[LENGTH_OFFSET..LENGTH_OFFSET + 8]
                .try_into()
                .unwrap(),
        );

        let flags = u32::from_le_bytes(
            param_buf[FLAGS_OFFSET..FLAGS_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        // --- Step 4: Determine pgprot from caching type --- The host driver may have updated the caching type in flags after the ioctl (see nvproxy's rmMapMemory comment about this).

        let caching_type = (flags >> FLAGS_CACHING_TYPE_SHIFT) & FLAGS_CACHING_TYPE_MASK;

        let native_cache = self.native_mapping_cache(
            host_fd,
            u32::from_le_bytes(param_buf[0..4].try_into().unwrap()),
            u32::from_le_bytes(param_buf[4..8].try_into().unwrap()),
            u32::from_le_bytes(param_buf[8..12].try_into().unwrap()),
        );
        let pgprot = if let Some(kind) = native_cache {
            kind
        } else {
            match caching_type {
                CACHING_TYPE_CACHED | CACHING_TYPE_WRITEBACK => PgprotKind::WriteBack,
                // System-memory mappings retain DEFAULT rather than returning their allocation's cache kind.
                CACHING_TYPE_DEFAULT => self
                    .memory_cache
                    .default_cache(
                        u32::from_le_bytes(param_buf[0..4].try_into().unwrap()),
                        u32::from_le_bytes(param_buf[8..12].try_into().unwrap()),
                    )
                    .unwrap_or(PgprotKind::WriteCombine),
                CACHING_TYPE_WRITECOMBINED => PgprotKind::WriteCombine,
                CACHING_TYPE_UNCACHED | CACHING_TYPE_UNCACHED_WEAK => PgprotKind::Uncached,
                other => {
                    log::warn!(
                        "NV_ESC_RM_MAP_MEMORY: unknown caching type {}, defaulting to UC",
                        other
                    );
                    PgprotKind::Uncached
                }
            }
        };

        // --- Step 5: Allocate SHM region ---

        let region = match self.shm.alloc(length, pgprot) {
            Ok(r) => r,
            Err(e) => {
                log::error!("NV_ESC_RM_MAP_MEMORY: SHM alloc failed: {}", e);
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
            }
        };

        // --- Step 6: place the device fd in the window --- Placed by the transport, not here: see `WindowPlacer`.
        if let Err(e) = window.place(region.offset, length, host_map_fd, 0, true) {
            log::error!("NV_ESC_RM_MAP_MEMORY: placing in the window failed: {}", e);
            if let Err(e) = self.shm.free(&region) {
                log::warn!("NV_ESC_RM_MAP_MEMORY: releasing failed placement: {e}");
            }
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        }

        log::debug!(
            "dispatch_map_memory: returning shm_offset=0x{:x} shm_length=0x{:x} pgprot={}",
            region.offset,
            length,
            pgprot as u8
        );

        // --- Step 6.5: Save host pLinearAddress and record mapping --- The host wrote its kernel VA into pLinearAddress (offset 32).

        // NVOS34.pLinearAddress is documented as "address of application mapping".
        let host_p_linear = u64::from_le_bytes(param_buf[32..40].try_into().unwrap());
        let h_client = u32::from_le_bytes(param_buf[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_buf[8..12].try_into().unwrap());

        // The handle is the key the mmap that follows will be found by, so it is the one field worth naming in the log: a mapping that is armed against one file and consumed on another is the whole failure mode.
        log::debug!(
            "MAP_MEMORY: armed on handle {} (shm_off={:#x}) → host_va={:#x} client={:#x} mem={:#x}",
            guest_fd_handle,
            region.offset,
            host_p_linear,
            h_client,
            h_memory
        );

        let region_offset = region.offset;
        self.active_maps.insert(
            region_offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: host_p_linear,
                shm_length: length,
                h_client,
                h_memory,
                map_fd_handle: guest_fd_handle,
                region,
            },
        );

        // Replace host VA with SHM offset in pLinearAddress — this is what the guest sees.
        param_buf[32..40].copy_from_slice(&region_offset.to_le_bytes());

        // --- Step 7: Respond --- Only the parameter buffer goes back.
        log::debug!(
            "map_memory: SHM {:#x}+{:#x}, pgprot {pgprot:?}",
            region_offset,
            length
        );
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    pub(super) fn dispatch_unmap_memory(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        if param_in.len() < 32 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let h_client = u32::from_le_bytes(param_in[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
        let guest_linear = u64::from_le_bytes(param_in[16..24].try_into().unwrap());

        // guest_linear is the SHM offset we wrote into pLinearAddress during map.
        let mapping_offset = self
            .active_maps
            .guest_offset(h_client, h_memory, guest_linear);
        let entry = match mapping_offset
            .and_then(|offset| self.active_maps.find_by_offset(offset))
            .copied()
        {
            Some(e) => e,
            None => {
                log::warn!(
                    "UNMAP_MEMORY: no mapping for pLinearAddress={:#x} \
                     (hClient={:#x}, hMemory={:#x})",
                    guest_linear,
                    h_client,
                    h_memory
                );
                // Forward with the guest value — host will reject but we report the error cleanly rather than crashing
                let mut param_buf = param_in.to_vec();
                if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
            }
        };

        log::debug!(
            "UNMAP_MEMORY: shm_off={:#x} → host_va={:#x} (client={:#x}, mem={:#x})",
            guest_linear,
            entry.host_p_linear_address,
            h_client,
            h_memory
        );

        // Substitute the real host pLinearAddress for the host ioctl
        let mut param_buf = param_in.to_vec();
        param_buf[16..24].copy_from_slice(&entry.host_p_linear_address.to_le_bytes());

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
            log::warn!("UNMAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore the entry since unmap didn't happen
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        let status = u32::from_le_bytes(param_buf[24..28].try_into().unwrap());
        log::debug!("UNMAP_MEMORY: host status=0x{:x}", status);

        if status == 0 {
            // Host unmap succeeded -- restore the SHM backing and return the extent to its zone, so the space can serve a later mapping.
            if let Err(e) = self.release_mapped_region(&entry.region) {
                log::warn!("UNMAP_MEMORY: SHM free failed: {} (non-fatal)", e);
            } else {
                self.active_maps.remove(entry.region.offset);
            }
        } else {
            // Host returned RM error — put the entry back
            log::warn!(
                "UNMAP_MEMORY: host RM status 0x{:x}, restoring mapping",
                status
            );
        }

        // Zero pLinearAddress in response — guest doesn't need it
        param_buf[16..24].copy_from_slice(&0u64.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }
}

#[cfg(test)]
mod mapping_reply_tests {
    use super::*;
    use crate::shm::PgprotKind;

    struct MappingHost(u32);

    impl HostDriver for MappingHost {
        fn ioctl(&self, _: RawFd, _: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
            assert_eq!(&arg[16..24], &0x1234_5000_u64.to_le_bytes());
            assert_eq!(&arg[24..32], &0x1234_5000_u64.to_le_bytes());
            arg[32..36].copy_from_slice(&self.0.to_le_bytes());
            Ok(())
        }
    }

    #[test]
    fn mapping_update_translates_host_inputs_and_preserves_caller_reply() {
        for status in [0_u32, 0x56] {
            let mut backend = NvidiaBackend::for_test();
            backend.set_host(Box::new(MappingHost(status)));
            backend.current_data_len = 40;
            let region = backend
                .shm
                .alloc(4096, PgprotKind::WriteBack)
                .expect("test extent");
            backend.active_maps.insert(
                region.offset,
                crate::mmap::MmapEntry {
                    host_p_linear_address: 0x1234_5000,
                    shm_length: 4096,
                    h_client: 17,
                    h_memory: 29,
                    map_fd_handle: 0,
                    region,
                },
            );
            let mut params = [0_u8; 40];
            params[0..4].copy_from_slice(&17_u32.to_le_bytes());
            params[8..12].copy_from_slice(&29_u32.to_le_bytes());
            params[16..24].copy_from_slice(&0x7fff_0000_1000_u64.to_le_bytes());
            params[24..32].copy_from_slice(&0x7fff_0000_9000_u64.to_le_bytes());
            let mut reply = [0_u8; 128];
            let written =
                backend.dispatch_update_device_mapping_info(0, -1, 0, &params, &mut reply);
            let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
            assert_eq!(written, head + params.len());
            params[32..36].copy_from_slice(&status.to_le_bytes());
            assert_eq!(&reply[head..written], &params);
        }
    }
}

#[cfg(test)]
mod mapping_unmap_tests {
    use super::*;
    use crate::shm::PgprotKind;

    struct UnmapHost {
        update_status: u32,
        unmap_status: u32,
        unmap_errno: Option<i32>,
    }

    impl HostDriver for UnmapHost {
        fn ioctl(&self, _: RawFd, _: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
            assert_eq!(&arg[16..24], &0x1234_5000_u64.to_le_bytes());
            if arg.len() == 40 {
                assert_eq!(&arg[24..32], &0x1234_5000_u64.to_le_bytes());
                arg[32..36].copy_from_slice(&self.update_status.to_le_bytes());
            } else {
                assert_eq!(arg.len(), 32);
                if let Some(errno) = self.unmap_errno {
                    return Err(errno);
                }
                arg[24..28].copy_from_slice(&self.unmap_status.to_le_bytes());
            }
            Ok(())
        }
    }

    #[test]
    fn updated_guest_va_unmaps_the_owned_host_mapping_and_recycles_extent() {
        for (status, errno) in [(0, None), (0x56, None), (0, Some(libc::EINVAL))] {
            let mut backend = NvidiaBackend::for_test();
            backend.set_host(Box::new(UnmapHost {
                update_status: 0,
                unmap_status: status,
                unmap_errno: errno,
            }));
            let before = backend.shm_free_bytes();
            let region = backend
                .shm
                .alloc(4096, PgprotKind::WriteBack)
                .expect("owned extent");
            backend.active_maps.insert(
                region.offset,
                crate::mmap::MmapEntry {
                    host_p_linear_address: 0x1234_5000,
                    shm_length: 4096,
                    h_client: 17,
                    h_memory: 29,
                    map_fd_handle: 0,
                    region,
                },
            );
            let mut update = [0_u8; 40];
            update[0..4].copy_from_slice(&17_u32.to_le_bytes());
            update[8..12].copy_from_slice(&29_u32.to_le_bytes());
            update[16..24].copy_from_slice(&region.offset.to_le_bytes());
            update[24..32].copy_from_slice(&0x7fff_0000_9000_u64.to_le_bytes());
            let mut reply = [0_u8; 128];
            backend.current_data_len = 40;
            backend.dispatch_update_device_mapping_info(0, -1, 0, &update, &mut reply);
            assert_eq!(
                backend.active_maps.guest_offset(17, 29, 0x7fff_0000_9000),
                Some(region.offset)
            );
            let mut unmap = [0_u8; 32];
            unmap[0..4].copy_from_slice(&17_u32.to_le_bytes());
            unmap[8..12].copy_from_slice(&29_u32.to_le_bytes());
            unmap[16..24].copy_from_slice(&0x7fff_0000_9000_u64.to_le_bytes());
            backend.current_data_len = 32;
            backend.dispatch_unmap_memory(0, -1, 0, &unmap, &mut reply);
            let succeeded = status == 0 && errno.is_none();
            assert_eq!(backend.active_maps.is_empty(), succeeded);
            assert_eq!(backend.shm_free_bytes() == before, succeeded);
            assert_eq!(
                backend
                    .active_maps
                    .guest_offset(17, 29, 0x7fff_0000_9000)
                    .is_none(),
                succeeded
            );
        }
    }

    #[test]
    fn failed_mapping_update_does_not_register_guest_address() {
        let mut backend = NvidiaBackend::for_test();
        backend.set_host(Box::new(UnmapHost {
            update_status: 0x56,
            unmap_status: 0,
            unmap_errno: None,
        }));
        let region = backend
            .shm
            .alloc(4096, PgprotKind::WriteBack)
            .expect("owned extent");
        backend.active_maps.insert(
            region.offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: 0x1234_5000,
                shm_length: 4096,
                h_client: 17,
                h_memory: 29,
                map_fd_handle: 0,
                region,
            },
        );
        let mut update = [0_u8; 40];
        update[0..4].copy_from_slice(&17_u32.to_le_bytes());
        update[8..12].copy_from_slice(&29_u32.to_le_bytes());
        update[16..24].copy_from_slice(&region.offset.to_le_bytes());
        update[24..32].copy_from_slice(&0x7fff_0000_9000_u64.to_le_bytes());
        let mut reply = [0_u8; 128];
        backend.current_data_len = 40;
        backend.dispatch_update_device_mapping_info(0, -1, 0, &update, &mut reply);
        assert_eq!(
            backend.active_maps.guest_offset(17, 29, 0x7fff_0000_9000),
            None
        );
        assert_eq!(backend.active_maps.len(), 1);
    }
}
