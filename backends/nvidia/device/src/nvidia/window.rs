//! MMAP and MUNMAP: placing device memory in the shared window.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------ MMAP / MUNMAP ------------------------------------------------------------------

    /// Place a mapping the guest asked for into the shared window.
    pub(super) fn handle_mmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<MmapReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL);
        }
        let req = read_struct::<MmapReq>(payload, 0);

        // A DRM node never takes the recorded path.
        if self.handle_kinds.get(&(self.current_handle as u64)) == Some(&DeviceKind::Uvm) {
            return self.map_uvm_pool(req.size, req.offset, resp_buf);
        }

        if matches!(
            self.handle_kinds.get(&(self.current_handle as u64)),
            Some(DeviceKind::Dri(_))
        ) {
            return self.map_unrecorded(req.size, req.offset, resp_buf);
        }

        let entry = match self
            .active_maps
            .find_by_fd_handle(self.current_handle as u64)
        {
            Some(e) => e,
            None => {
                // Bookkeeping here records what RM_MAP_MEMORY armed, and that is not the only ioctl that arms a mapping: NV_ESC_RM_ALLOC_MEMORY names a file in the same way and arms it too, which is what the descriptor at the end of its parameters is for.
                return self.map_unrecorded(req.size, req.offset, resp_buf);
            }
        };

        // What goes back is the offset within the window, not a guest physical address.
        let (offset, length) = (entry.region.offset, entry.region.length);
        // A mapping is rarely a whole number of pages -- the usermode aperture is 0xc70 bytes -- and mmap always covers whole pages, so the guest asking for more than the mapping holds is the normal case and not an overrun.
        let page = 4096u64;
        let mapped_pages = length.div_ceil(page) * page;
        if req.size > mapped_pages {
            log::warn!(
                "mmap: guest asked for {:#x} bytes of a {length:#x}-byte mapping at {offset:#x}",
                req.size
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }

        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: offset,
                size: mapped_pages,
                mapping_id: 0,
                padding: 0x4c47_0000 | entry.region.pgprot as u32,
            },
        );
        log::debug!("mmap: window offset {offset:#x}+{length:#x}");
        off
    }

    /// Serve an mmap on a file this backend has no record of arming.
    pub(super) fn map_unrecorded(&mut self, size: u64, offset: u64, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle as u64;
        let host_fd = match self.handles.get_raw(handle) {
            Ok(fd) => fd,
            Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::ENOENT),
        };

        // Caching follows the device, which is the same rule the recorded path reaches through the flags RM returns: the control device carries system memory, and a GPU device carries the card's own.
        let kind = self.handle_kinds.get(&handle).copied();
        let pgprot = match kind {
            Some(DeviceKind::Gpu(_)) | Some(DeviceKind::Dri(_)) => {
                crate::shm::PgprotKind::WriteCombine
            }
            _ => crate::shm::PgprotKind::WriteBack,
        };

        // On a DRM node the guest's offset is a real position in the file -- GEM_MAP_OFFSET issued it, on this very descriptor -- and the object's memory is reachable nowhere else.
        let fd_offset = match kind {
            Some(DeviceKind::Dri(_)) => offset,
            _ => 0,
        };

        // The same object mapped twice is the same memory: hand back the placement that is already there rather than a second copy of it.
        if let Some(&id) = self.dri_maps.get(&(handle, fd_offset))
            && let Some(live) = self.live_maps.get(&id)
        {
            let (offset, length) = (live.region.offset, live.length);
            let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
            if resp_buf.len() < need {
                return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
            }
            let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
            off += write_struct(
                &mut resp_buf[off..],
                &MmapResp {
                    guest_phys_addr: offset,
                    size: length.div_ceil(4096) * 4096,
                    mapping_id: id,
                    padding: 0x4c47_0000 | live.region.pgprot as u32,
                },
            );
            return off;
        }

        let Some(window) = self.window.as_ref() else {
            log::warn!("mmap on handle {handle}: no shared window to place it in");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOTSUP);
        };

        let length = size.max(4096);
        let region = match self.shm.alloc(length, pgprot) {
            Ok(r) => r,
            Err(e) => {
                log::error!("mmap on handle {handle}: window has no room: {e}");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOMEM);
            }
        };

        if let Err(e) = window.place(region.offset, length, host_fd, fd_offset, true) {
            log::warn!(
                "mmap on handle {handle}: nothing armed on this file, or it could \
                 not be placed: {e}"
            );
            if let Err(e) = self.shm.free(&region) {
                log::warn!("mmap on handle {handle}: releasing failed placement: {e}");
            }
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }

        log::debug!(
            "mmap on handle {handle}: placed {length:#x} bytes at window offset {:#x} \
             with no arming recorded here",
            region.offset
        );

        let offset = region.offset;
        let id = self.next_mapping_id;
        self.next_mapping_id = self.next_mapping_id.wrapping_add(1).max(1);
        self.dri_maps.insert((handle, fd_offset), id);
        self.live_maps.insert(
            id,
            LiveMap {
                key: (handle, fd_offset),
                region,
                length,
            },
        );
        self.active_maps.insert(
            offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: 0,
                shm_length: length,
                h_client: 0,
                h_memory: 0,
                map_fd_handle: handle,
                region,
            },
        );

        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: offset,
                size: length.div_ceil(4096) * 4096,
                mapping_id: id,
                padding: 0x4c47_0000 | pgprot as u32,
            },
        );
        off
    }

    pub(super) fn handle_munmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<MunmapReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL);
        }
        let req = read_struct::<MunmapReq>(payload, 0);

        if let Some(pool) = self.aperture.remove(req.mapping_id) {
            self.unmap_uvm_pool(req.mapping_id, pool);
            return self.write_hdr(resp_buf, 0, 0);
        }

        // Zero is what every mapping the RM path hands out carries: those are taken back by RM_UNMAP_MEMORY, which names them by the address in pLinearAddress.
        let Some(live) = self.live_maps.remove(&req.mapping_id) else {
            return self.write_hdr(resp_buf, 0, 0);
        };
        self.dri_maps.remove(&live.key);

        // Emptied rather than unmapped: a hole would leave the memory slot covering a range that reaches no mapping at all, and a stray access there faults the VMM rather than the guest.
        if let Some(window) = self.window.as_ref()
            && let Err(e) = window.withdraw(live.region.offset, live.length)
        {
            log::warn!(
                "munmap {}: the window would not give it back: {e}",
                req.mapping_id
            );
        }
        self.active_maps.remove(live.region.offset);
        if let Err(e) = self.shm.free(&live.region) {
            log::warn!("munmap {}: freeing the window region: {e}", req.mapping_id);
        }
        log::debug!(
            "munmap {}: window offset {:#x}+{:#x} is free again",
            req.mapping_id,
            live.region.offset,
            live.length
        );
        self.write_hdr(resp_buf, 0, 0)
    }
}
