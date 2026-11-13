//! Active guest mappings.

use std::collections::HashMap;

use crate::shm::ShmRegion;

/// One live mapping.
#[derive(Debug, Clone, Copy)]
pub struct MmapEntry {
    /// The host's `pLinearAddress`, substituted back in on unmap.
    pub host_p_linear_address: u64,
    /// Length the guest asked for, before page rounding.
    pub shm_length: u64,
    pub h_client: u32,
    pub h_memory: u32,
    /// The guest handle whose host fd carries this mapping.
    pub map_fd_handle: u64,
    /// What to hand back to the SHM allocator when this mapping goes away.
    pub region: ShmRegion,
}

/// Live mappings, indexed by SHM offset.
#[derive(Default)]
pub struct MmapContext {
    entries: HashMap<u64, MmapEntry>,
    guest_addresses: HashMap<(u32, u32, u64), u64>,
}

impl MmapContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, shm_offset: u64, entry: MmapEntry) {
        self.remove(shm_offset);
        self.entries.insert(shm_offset, entry);
    }

    pub fn remove(&mut self, shm_offset: u64) -> Option<MmapEntry> {
        self.guest_addresses
            .retain(|_, offset| *offset != shm_offset);
        self.entries.remove(&shm_offset)
    }

    /// The mapping at a window offset.
    pub fn find_by_offset(&self, shm_offset: u64) -> Option<&MmapEntry> {
        self.entries.get(&shm_offset)
    }

    /// The mapping made on a given descriptor.
    pub fn find_by_fd_handle(&self, fd_handle: u64) -> Option<&MmapEntry> {
        self.entries.values().find(|e| e.map_fd_handle == fd_handle)
    }

    /// Take every mapping, leaving the table empty.
    pub fn drain(&mut self) -> Vec<MmapEntry> {
        self.guest_addresses.clear();
        self.entries.drain().map(|(_, e)| e).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Take every mapping carried by one guest handle.
    pub fn take_for_fd(&mut self, fd_handle: u64) -> Vec<MmapEntry> {
        let keys: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.map_fd_handle == fd_handle)
            .map(|(k, _)| *k)
            .collect();
        keys.into_iter().filter_map(|k| self.remove(k)).collect()
    }

    /// Whether this handle already carries a mapping.
    pub fn fd_has_mapping(&self, fd_handle: u64) -> bool {
        self.entries.values().any(|e| e.map_fd_handle == fd_handle)
    }

    /// Find the mapping for a given client/memory pair.
    pub fn find_by_object(&self, h_client: u32, h_memory: u32) -> Option<&MmapEntry> {
        self.entries
            .values()
            .find(|e| e.h_client == h_client && e.h_memory == h_memory)
    }
}

// Guest virtual addresses are opaque lookup keys, never host pointers.
impl MmapContext {
    pub(super) fn guest_offset(&self, client: u32, memory: u32, address: u64) -> Option<u64> {
        let matches = |entry: &MmapEntry| entry.h_client == client && entry.h_memory == memory;
        if self.entries.get(&address).is_some_and(matches) {
            return Some(address);
        }
        let offset = *self.guest_addresses.get(&(client, memory, address))?;
        self.entries
            .get(&offset)
            .filter(|entry| matches(entry))
            .map(|_| offset)
    }

    /// Resolve an initial mapping update only when object identity is unique.
    pub(super) fn update_offset(&self, client: u32, memory: u32, address: u64) -> Option<u64> {
        if let Some(offset) = self.guest_offset(client, memory, address) {
            return Some(offset);
        }
        let mut candidates = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.h_client == client && entry.h_memory == memory);
        let (offset, _) = candidates.next()?;
        candidates.next().is_none().then_some(*offset)
    }

    pub(super) fn note_guest_address(&mut self, offset: u64, address: u64) {
        let Some(entry) = self.entries.get(&offset) else {
            return;
        };
        let key = (entry.h_client, entry.h_memory, address);
        self.guest_addresses.retain(|_, target| *target != offset);
        if address != 0 {
            self.guest_addresses.insert(key, offset);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/mmap_guest_address_tests.rs"]
mod guest_address_tests;
