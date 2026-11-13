//! CPU cache metadata for successful NVIDIA RM memory allocations.
use crate::shm::PgprotKind;
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Object {
    parent: u32,
    write_back: bool,
    native_uncached: bool,
    class: u32,
    default_cache: Option<PgprotKind>,
}

#[derive(Default)]
pub(crate) struct CachedObjects {
    objects: HashMap<(u32, u32), Object>,
}

impl CachedObjects {
    pub(crate) fn allocate(&mut self, client: u32, parent: u32, object: u32, write_back: bool) {
        // A reused numeric handle must never inherit another generation's kind.
        self.free(client, object);
        self.objects.insert(
            (client, object),
            Object {
                parent,
                write_back,
                native_uncached: false,
                class: 0,
                default_cache: write_back.then_some(PgprotKind::WriteBack),
            },
        );
    }

    pub(crate) fn allocation_attributes(&mut self, client: u32, object: u32, attribute: u32) {
        if let Some(entry) = self.objects.get_mut(&(client, object)) {
            entry.default_cache = match (attribute >> 29) & 7 {
                0 => Some(PgprotKind::Uncached),
                1 | 5 => Some(PgprotKind::WriteBack),
                2 => Some(PgprotKind::WriteCombine),
                _ => None,
            };
        }
    }

    pub(crate) fn default_cache(&self, client: u32, object: u32) -> Option<PgprotKind> {
        self.objects
            .get(&(client, object))
            .and_then(|entry| entry.default_cache)
    }

    /// Native NVIDIA maps the CPU doorbell as uncached register memory, regardless of the DEFAULT field returned by RM_MAP_MEMORY.
    pub(crate) fn allocation_class(&mut self, client: u32, object: u32, class: u32) {
        if let Some(entry) = self.objects.get_mut(&(client, object)) {
            entry.class = class;
            entry.native_uncached = matches!(class, 0xc361 | 0xc461 | 0xc561 | 0xc661 | 0xc761);
            // RM_USER_SHARED_DATA (cl00de.h) wraps cached system memory: gpuCreateRusdMemory_IMPL creates ADDR_SYSMEM/NV_MEMORY_CACHED.
            if class == 0xde {
                if std::env::var_os("LEGION_RUSD_CACHE_CONTROL").is_none() {
                    entry.default_cache = Some(PgprotKind::WriteBack);
                }
                log::info!("RUSD default cache: {:?}", entry.default_cache);
            }
        }
    }

    pub(crate) fn native_uncached(&self, client: u32, object: u32) -> bool {
        self.objects
            .get(&(client, object))
            .is_some_and(|v| v.native_uncached)
    }

    pub(crate) fn is_userd(&self, client: u32, object: u32) -> bool {
        self.objects.get(&(client, object)).is_some_and(|entry| {
            matches!(
                entry.class,
                0x906f
                    | 0xa06f
                    | 0xa16f
                    | 0xb06f
                    | 0xc06f
                    | 0xc36f
                    | 0xc46f
                    | 0xc56f
                    | 0xc86f
                    | 0xc96f
                    | 0xca6f
            )
        })
    }

    pub(crate) fn duplicate(
        &mut self,
        client: u32,
        parent: u32,
        object: u32,
        source_client: u32,
        source: u32,
    ) {
        let source = self.objects.get(&(source_client, source)).copied();
        self.allocate(client, parent, object, source.is_some_and(|v| v.write_back));
        if let Some(entry) = self.objects.get_mut(&(client, object)) {
            entry.native_uncached = source.is_some_and(|v| v.native_uncached);
            entry.class = source.map_or(0, |v| v.class);
            entry.default_cache = source.and_then(|v| v.default_cache);
        }
    }

    pub(crate) fn free(&mut self, client: u32, object: u32) {
        if client == object {
            self.objects.retain(|&(owner, _), _| owner != client);
            return;
        }
        let mut pending = vec![object];
        while let Some(object) = pending.pop() {
            self.objects.remove(&(client, object));
            let children: Vec<_> = self
                .objects
                .iter()
                .filter(|&(&(owner, _), entry)| owner == client && entry.parent == object)
                .map(|(&(_, handle), _)| handle)
                .collect();
            for child in children {
                self.objects.remove(&(client, child));
                pending.push(child);
            }
        }
    }
}

pub(crate) fn cached_system_memory(attribute: u32, location: (u32, u32)) -> bool {
    let (low, high) = location;
    if low > high || high >= 32 {
        return false;
    }
    let width = high - low + 1;
    let mask = u32::MAX >> (32 - width);
    let location = (attribute >> low) & mask;
    let coherency = (attribute >> 29) & 7;
    location == 1 && matches!(coherency, 1 | 5)
}

#[cfg(test)]
#[path = "../../tests/unit/nvidia/legion_memory_cache.rs"]
mod tests;
