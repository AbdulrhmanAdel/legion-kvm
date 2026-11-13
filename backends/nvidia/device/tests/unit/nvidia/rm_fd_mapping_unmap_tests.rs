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
