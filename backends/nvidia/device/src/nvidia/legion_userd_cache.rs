//! Ask the native driver for USERD caching instead of assuming DEFAULT means WC.
use super::*;
use crate::shm::PgprotKind;

impl NvidiaBackend {
    pub(super) fn native_mapping_cache(
        &self,
        fd: RawFd,
        client: u32,
        device: u32,
        object: u32,
    ) -> Option<PgprotKind> {
        if self.memory_cache.native_uncached(client, object) {
            return Some(PgprotKind::Uncached);
        }
        if !self.memory_cache.is_userd(client, object) {
            return None;
        }
        // NV2080_CTRL_CMD_FIFO_GET_USERD_LOCATION returns aperture + attribute.
        let mut location = [u32::MAX; 2];
        let mut outer = [0_u8; 32];
        outer[..4].copy_from_slice(&client.to_le_bytes());
        outer[4..8].copy_from_slice(&device.to_le_bytes());
        outer[8..12].copy_from_slice(&0x2080_110d_u32.to_le_bytes());
        outer[16..24].copy_from_slice(&(location.as_mut_ptr() as u64).to_le_bytes());
        outer[24..28].copy_from_slice(&8_u32.to_le_bytes());
        // The synchronous host call receives only this owned 8-byte array; its pointer never enters a guest request or reply.
        if self
            .host
            .ioctl(fd, abi::ioctl::_IOWR(0x2a, 32), &mut outer)
            .is_err()
            || outer[28..32] != [0; 4]
        {
            log::warn!(
                "USERD cache query failed for {client:#x}/{device:#x}; retaining returned mapping flags"
            );
            return None;
        }
        let kind = match location[1] {
            0 => PgprotKind::WriteBack,
            1 => PgprotKind::Uncached,
            2 => PgprotKind::WriteCombine,
            _ => return None,
        };
        log::info!(
            "native USERD cache client={client:#x} object={object:#x} aperture={} cache={kind:?}",
            location[0]
        );
        Some(kind)
    }
}
