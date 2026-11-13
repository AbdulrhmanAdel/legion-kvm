use super::*;
use crate::shm::{PgprotKind, ShmRegion};

fn entry(offset: u64, client: u32, memory: u32, fd: u64) -> MmapEntry {
    MmapEntry {
        host_p_linear_address: 0x1234_5000,
        shm_length: 4096,
        h_client: client,
        h_memory: memory,
        map_fd_handle: fd,
        region: ShmRegion {
            offset,
            length: 4096,
            pgprot: PgprotKind::WriteBack,
        },
    }
}

#[test]
fn guest_addresses_are_scoped_and_retired_with_the_mapping() {
    let mut maps = MmapContext::new();
    maps.insert(4096, entry(4096, 1, 2, 3));
    maps.note_guest_address(4096, 0x7fff_0000);
    assert_eq!(maps.guest_offset(1, 2, 0x7fff_0000), Some(4096));
    assert_eq!(maps.guest_offset(9, 2, 0x7fff_0000), None);
    assert_eq!(maps.guest_offset(1, 9, 4096), None);
    maps.note_guest_address(4096, 0x7fff_9000);
    assert_eq!(maps.guest_offset(1, 2, 0x7fff_0000), None);
    maps.remove(4096);
    maps.insert(4096, entry(4096, 1, 2, 4));
    assert_eq!(maps.guest_offset(1, 2, 0x7fff_9000), None);
    maps.note_guest_address(4096, 0x7fff_9000);
    maps.take_for_fd(4);
    assert!(maps.guest_addresses.is_empty());
    maps.insert(4096, entry(4096, 1, 2, 4));
    maps.note_guest_address(4096, 0x7fff_9000);
    maps.drain();
    assert!(maps.guest_addresses.is_empty());
}

#[test]
fn duplicate_object_mappings_require_an_address_match() {
    let mut maps = MmapContext::new();
    maps.insert(4096, entry(4096, 1, 2, 3));
    maps.insert(8192, entry(8192, 1, 2, 4));
    assert_eq!(maps.update_offset(1, 2, 123), None);
    assert_eq!(maps.update_offset(1, 2, 8192), Some(8192));
    maps.note_guest_address(8192, 0x7fff_9000);
    assert_eq!(maps.update_offset(1, 2, 0x7fff_9000), Some(8192));
}
