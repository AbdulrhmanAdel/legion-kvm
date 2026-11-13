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
        let written = backend.dispatch_update_device_mapping_info(0, -1, 0, &params, &mut reply);
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        assert_eq!(written, head + params.len());
        params[32..36].copy_from_slice(&status.to_le_bytes());
        assert_eq!(&reply[head..written], &params);
    }
}
