use super::*;

#[test]
fn the_releases_agree_about_where_an_address_is() {
    let first = (PROFILES[0].osdesc)();
    for p in PROFILES {
        assert_eq!(
            (p.osdesc)(),
            first,
            "{} describes registration by address differently",
            p.version
        );
    }
}

/// Reading the address out of a block shorter than the release's own would read whatever follows it, so every route refuses to.
#[test]
fn an_address_is_only_read_from_a_block_long_enough_to_hold_one() {
    let d = (PROFILES[0].osdesc)();
    for r in [d.alloc, d.alloc_memory, d.vid_heap] {
        assert!(r.address_at + 8 <= r.params_size);
        assert!(r.limit_at + 8 <= r.params_size);
        let full = vec![0u8; r.params_size];
        assert_eq!(r.address(&full), Some(0));
        let short = vec![0u8; r.address_at + 7];
        assert_eq!(r.address(&short), None);
    }
}

/// `limit` is the last byte's offset, not the length.
#[test]
fn a_limit_is_one_less_than_the_length() {
    let d = (PROFILES[0].osdesc)();
    let mut p = vec![0u8; d.alloc.params_size];
    p[d.alloc.limit_at..d.alloc.limit_at + 8].copy_from_slice(&4095u64.to_le_bytes());
    assert_eq!(d.alloc.length(&p), Some(4096));

    // And a limit of u64::MAX has no length at all rather than wrapping to zero, which would otherwise read as "register nothing".
    p[d.alloc.limit_at..d.alloc.limit_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(d.alloc.length(&p), None);
}

/// The union is only an address when `function` says so.
#[test]
fn a_heap_call_is_only_a_registration_when_its_function_says_so() {
    let d = (PROFILES[0].osdesc)();
    let mut p = vec![0u8; d.vid_heap.params_size];
    assert!(!d.vid_heap_registers_address(&p));
    let at = d.vid_heap_function_at;
    p[at..at + 4].copy_from_slice(&d.vid_heap_function.to_le_bytes());
    assert!(d.vid_heap_registers_address(&p));
}

#[test]
fn every_descriptor_type_has_a_name_to_refuse_it_by() {
    let d = (PROFILES[0].osdesc)();
    assert_eq!(
        d.type_name(d.virtual_address),
        Some("NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS")
    );
    assert!(d.types.len() >= 8, "{} types", d.types.len());
    assert_eq!(d.type_name(0xdead), None);
}

#[test]
fn a_release_older_than_every_table_has_none() {
    assert!(select(DriverVersion::new(470, 0, 0)).is_none());
    assert!(select(DriverVersion::new(615, 71, 9)).is_some());
    // Nearest-older, as everywhere else.
    assert_eq!(
        select(DriverVersion::new(600, 0, 0)),
        Some(v580_178_04::OSDESC)
    );
}
