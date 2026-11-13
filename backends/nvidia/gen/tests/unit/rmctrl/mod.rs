use super::*;

/// `lookup` is a binary search, so this is not a style point.
#[test]
fn every_table_is_sorted_and_has_no_duplicate() {
    for p in PROFILES {
        for w in (p.table)().windows(2) {
            assert!(
                w[0].cmd < w[1].cmd,
                "{}: {:#010x} then {:#010x}",
                p.version,
                w[0].cmd,
                w[1].cmd
            );
        }
    }
}

#[test]
fn a_refused_entry_describes_nothing() {
    for p in PROFILES {
        for e in (p.table)() {
            if e.refuse {
                assert!(e.ptrs.is_empty(), "{}: {:#010x}", p.version, e.cmd);
            }
        }
    }
}

/// Every pointer has to sit inside the block it is read from, and address something.
#[test]
fn every_pointer_fits_its_parameter_block() {
    for p in PROFILES {
        for e in (p.table)().iter().filter(|e| !e.refuse) {
            for ptr in e.ptrs {
                assert!(
                    ptr.ptr_offset + 8 <= e.params_size,
                    "{}: {:#010x} pointer at {} of {} bytes",
                    p.version,
                    e.cmd,
                    ptr.ptr_offset,
                    e.params_size
                );
                assert!(ptr.elem_size > 0, "{}: {:#010x}", p.version, e.cmd);
                assert!(
                    ptr.copy_in || ptr.copy_out,
                    "{}: {:#010x} copies in neither direction",
                    p.version,
                    e.cmd
                );
                if let Count::Field { offset, width } | Count::Squared { offset, width } = ptr.count
                {
                    assert!(
                        offset + width <= e.params_size,
                        "{}: {:#010x} count at {offset}",
                        p.version,
                        e.cmd
                    );
                    assert!(matches!(width, 1 | 2 | 4 | 8));
                }
            }
        }
    }
}

/// The control the guest driver carried by hand before this table existed: `NV0041_CTRL_CMD_GET_SURFACE_INFO`, a list of eight-byte entries whose count leads the block.
#[test]
fn get_surface_info_is_a_list_of_eight_byte_entries() {
    let t = table_for(DriverVersion::new(615, 71, 9)).expect("615 has a table");
    let e = lookup(t, 0x0041_0110).expect("GET_SURFACE_INFO is in the table");
    assert_eq!(e.ptrs.len(), 1);
    let p = e.ptrs[0];
    assert_eq!(p.ptr_offset, 8);
    assert_eq!(p.elem_size, 8);
    assert_eq!(
        p.count,
        Count::Field {
            offset: 0,
            width: 4
        }
    );

    // Three entries, as the leading count says.
    let mut params = vec![0u8; e.params_size];
    params[0..4].copy_from_slice(&3u32.to_le_bytes());
    assert_eq!(p.bytes(&params), Some(24));
}

/// A count a guest can write is a count a guest can lie about.
#[test]
fn an_impossible_count_refuses_rather_than_allocating() {
    let t = table_for(DriverVersion::new(615, 71, 9)).expect("615 has a table");
    let e = lookup(t, 0x0041_0110).expect("GET_SURFACE_INFO is in the table");
    let p = e.ptrs[0];

    let mut params = vec![0u8; e.params_size];
    params[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(p.bytes(&params), None);

    // A block too short to hold the count field is refused, not read.
    assert_eq!(p.bytes(&[]), None);
}

#[test]
fn a_zero_count_asks_for_nothing() {
    let t = table_for(DriverVersion::new(615, 71, 9)).expect("615 has a table");
    let e = lookup(t, 0x0041_0110).expect("GET_SURFACE_INFO is in the table");
    let params = vec![0u8; e.params_size];
    assert_eq!(e.ptrs[0].bytes(&params), Some(0));
}

/// `USE_AMAPLIB` is an RM build flag, so whether RM dereferences this control's pointers cannot be read off the headers.
#[test]
fn the_control_behind_a_build_flag_is_present_and_refused() {
    for v in supported_versions() {
        let t = table_for(v).expect("a listed version has a table");
        let e = lookup(t, 0x2080_1336).expect("FB_GET_AMAP_CONF is in every table");
        assert!(e.refuse, "{v}");
    }
}

/// The releases here differ by five or six controls, so this is the case a host between two tables lands in.
#[test]
fn a_control_another_release_describes_is_known_to_be_one() {
    // 0x00801b01 is described by 615 and by no older table.
    assert!(in_any_table(0x0080_1b01));
    let older = table_for(DriverVersion::new(595, 104, 2)).expect("595.104.02 has a table");
    assert!(lookup(older, 0x0080_1b01).is_none());

    // Something no release describes stays unknown.
    assert!(!in_any_table(0xdead_beef));
}

#[test]
fn exactness_is_reported_so_a_fallback_can_be_treated_differently() {
    assert!(
        select(DriverVersion::new(615, 71, 9))
            .expect("has one")
            .exact
    );
    assert!(
        !select(DriverVersion::new(600, 0, 0))
            .expect("falls back")
            .exact
    );
    assert!(
        !select(DriverVersion::new(700, 0, 0))
            .expect("falls back")
            .exact
    );
}

#[test]
fn a_version_between_tables_takes_the_lower_one() {
    let t = table_for(DriverVersion::new(600, 0, 0)).expect("falls back to 595.104.02");
    assert!(std::ptr::eq(t, v595_104_02::TABLE));
    assert!(table_for(DriverVersion::new(470, 0, 0)).is_none());
}

/// Squaring is where an overflow would hide: `gpuCount` is a `u32`.
#[test]
fn a_squared_count_cannot_overflow_into_a_small_number() {
    let c = Count::Squared {
        offset: 0,
        width: 4,
    };
    let params = u32::MAX.to_le_bytes().to_vec();
    assert_eq!(c.elements(&params), Some(18_446_744_065_119_617_025));
    let p = EmbeddedPtr {
        ptr_offset: 0,
        count: c,
        elem_size: 4,
        copy_in: false,
        copy_out: true,
    };
    assert_eq!(p.bytes(&params), None);
}
