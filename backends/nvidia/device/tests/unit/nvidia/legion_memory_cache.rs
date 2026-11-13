use super::*;

#[test]
fn shared_driver_data_keeps_native_caching_across_duplicate_and_reuse() {
    let mut objects = CachedObjects::default();
    objects.allocate(1, 1, 10, false);
    objects.allocation_class(1, 10, 0xde);
    assert_eq!(objects.default_cache(1, 10), Some(PgprotKind::WriteBack));
    assert!(!objects.native_uncached(1, 10));
    objects.duplicate(2, 2, 20, 1, 10);
    assert_eq!(objects.default_cache(2, 20), Some(PgprotKind::WriteBack));
    objects.free(1, 1);
    assert_eq!(objects.default_cache(1, 10), None);
    assert_eq!(objects.default_cache(2, 20), Some(PgprotKind::WriteBack));
    objects.allocate(2, 2, 20, false);
    objects.allocation_class(2, 20, 0x40);
    assert_eq!(objects.default_cache(2, 20), None);
}

#[test]
fn default_mapping_retains_uncached_vidmem_and_wc_system_allocations() {
    let mut objects = CachedObjects::default();
    for (attribute, expected) in [
        (0x11000000, PgprotKind::Uncached),
        (0x4a800000, PgprotKind::WriteCombine),
        (0x2a800000, PgprotKind::WriteBack),
    ] {
        objects.allocate(1, 1, 10, false);
        objects.allocation_attributes(1, 10, attribute);
        assert_eq!(objects.default_cache(1, 10), Some(expected));
        objects.duplicate(2, 2, 20, 1, 10);
        objects.free(1, 10);
        assert_eq!(objects.default_cache(2, 20), Some(expected));
        objects.allocate(2, 2, 20, false);
        assert_eq!(objects.default_cache(2, 20), None);
    }
}

#[test]
fn native_doorbell_cache_kind_follows_sdk_class_and_object_lifetime() {
    let mut objects = CachedObjects::default();
    for class in [0xc361, 0xc461, 0xc561, 0xc661, 0xc761] {
        objects.allocate(1, 1, 10, false);
        objects.allocation_class(1, 10, class);
        assert!(objects.native_uncached(1, 10));
        objects.duplicate(2, 2, 20, 1, 10);
        objects.free(1, 10);
        assert!(objects.native_uncached(2, 20));
        objects.allocate(2, 2, 20, true);
        objects.allocation_class(2, 20, 0x40);
        assert!(!objects.native_uncached(2, 20));
        assert!(objects.write_back(2, 20));
    }
    assert!(!objects.native_uncached(7, 10));
}

#[test]
fn the_observed_counter_is_cached_system_memory() {
    assert!(cached_system_memory(0x2a800000, (25, 26)));
    assert!(cached_system_memory((1 << 25) | (5 << 29), (25, 26)));
    for attr in [0, 1 << 25, (1 << 25) | (2 << 29), (1 << 29), u32::MAX] {
        assert!(!cached_system_memory(attr, (25, 26)));
    }
    assert!(!cached_system_memory(0x2a800000, (26, 25)));
    assert!(!cached_system_memory(0x2a800000, (25, 32)));
}

#[test]
fn freeing_an_ancestor_clears_descendants_and_preserves_other_clients() {
    let mut objects = CachedObjects::default();
    objects.allocate(1, 1, 10, false);
    objects.allocate(1, 10, 20, true);
    objects.allocate(1, 20, 30, true);
    objects.allocate(2, 2, 20, true);
    objects.free(1, 10);
    assert!(!objects.write_back(1, 20));
    assert!(!objects.write_back(1, 30));
    assert!(objects.write_back(2, 20));
    objects.free(2, 2);
    assert!(!objects.write_back(2, 20));
}

#[test]
fn aliases_survive_source_release_and_reuse_never_retains_a_kind() {
    let mut objects = CachedObjects::default();
    objects.allocate(1, 1, 10, true);
    objects.duplicate(2, 2, 20, 1, 10);
    objects.free(1, 10);
    assert!(objects.write_back(2, 20));
    objects.allocate(2, 2, 20, false);
    assert!(!objects.write_back(2, 20));
    objects.duplicate(3, 3, 30, 9, 999);
    assert!(!objects.write_back(3, 30));
}

impl CachedObjects {
    pub(crate) fn write_back(&self, client: u32, object: u32) -> bool {
        self.objects
            .get(&(client, object))
            .is_some_and(|v| v.write_back)
    }
}
