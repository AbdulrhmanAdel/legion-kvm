use super::*;

fn all() -> impl Iterator<Item = (DriverVersion, &'static Layout)> {
    PROFILES.iter().copied()
}

#[test]
fn profiles_are_in_ascending_order() {
    assert!(PROFILES.windows(2).all(|w| w[0].0 < w[1].0));
}

/// Two names on one index would have a clamp write one field twice.
#[test]
fn no_two_memory_indices_share_a_number() {
    for (v, l) in all() {
        let mut seen = std::collections::HashSet::new();
        for i in l.fb_info {
            assert!(seen.insert(i.index), "{v}: index {:#x} twice", i.index);
        }
    }
}

/// What a clamp cannot do without: every release has to say how much there is and how much is free.
#[test]
fn every_release_reports_a_total_and_a_free() {
    for (v, l) in all() {
        for name in ["HEAP_SIZE", "TOTAL_RAM_SIZE", "RAM_SIZE", "HEAP_FREE"] {
            assert!(l.index(name).is_some(), "{v}: no {name}");
        }
    }
}

#[test]
fn a_classified_index_is_either_present_or_named_absent() {
    let names = |l: &Layout| l.fb_info.iter().map(|i| i.name).collect::<Vec<_>>();
    let first = names(PROFILES[0].1).len() + PROFILES[0].1.fb_absent.len();
    for (v, l) in all() {
        assert_eq!(names(l).len() + l.fb_absent.len(), first, "{v}");
        for a in l.fb_absent {
            assert!(l.index(a).is_none(), "{v}: {a} both present and absent");
        }
    }
}

/// HEAP_RECLAIMABLE is the index the fork found moving.
#[test]
fn heap_reclaimable_is_absent_before_595_104() {
    for (v, l) in all() {
        let has = l.index("HEAP_RECLAIMABLE").is_some();
        assert_eq!(has, v >= DriverVersion::new(595, 104, 2), "{v}");
    }
}

/// V2's list is inline, so its block is the count and the full list.
#[test]
fn the_v2_block_is_its_count_and_its_whole_list() {
    for (v, l) in all() {
        let b = l.fb_get_info_v2;
        assert_eq!(
            b.size,
            b.list + l.fb_info_max_list * l.fb_info_entry_size,
            "{v}"
        );
    }
}

#[test]
fn the_two_spellings_are_the_ones_rm_names() {
    for (v, l) in all() {
        assert_eq!(l.cmd_fb_get_info, 0x2080_1301, "{v}");
        assert_eq!(l.cmd_fb_get_info_v2, 0x2080_1303, "{v}");
    }
}

#[test]
fn video_memory_is_told_from_system_and_virtual() {
    let l = &v615_71_09::LAYOUT;
    let (lo, _) = l.attr_location;
    assert!(l.takes_vidmem(l.attr_location_vidmem << lo, 0));
    assert!(!l.takes_vidmem(1 << lo, 0), "PCI, system memory");
    assert!(!l.takes_vidmem(0, l.alloc_flags_virtual));
}

#[test]
fn a_release_between_profiles_gets_the_older_one_inexactly() {
    let s = select(DriverVersion::new(600, 0, 0)).expect("a profile");
    assert!(!s.exact);
    assert!(std::ptr::eq(s.layout, &v595_104_02::LAYOUT));
    assert!(select(DriverVersion::new(615, 71, 9)).unwrap().exact);
    assert!(select(DriverVersion::new(500, 0, 0)).is_none());
}
