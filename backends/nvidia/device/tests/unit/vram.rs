use super::*;

const C: Client = 0xc1d0_0001;
const DEV: Handle = 0xcaf0_0001;
const SUB: Handle = 0xcaf0_0002;

/// A client with a device and a subdevice, as every RM user makes first.
fn with_device(limit_mib: Option<u64>) -> Vram {
    let mut v = Vram::new(limit_mib);
    v.allocated(C, C, C, None);
    v.allocated(C, C, DEV, None);
    v.allocated(C, DEV, SUB, None);
    v
}

#[test]
fn without_a_limit_everything_fits_and_is_still_counted() {
    let mut v = with_device(None);
    assert!(v.admit(u64::MAX));
    v.allocated(C, SUB, 0x10, Some(64 * MIB));
    assert_eq!(v.in_use(), 64 * MIB);
    assert_eq!(v.limit_mib(), 0);
    assert_eq!(v.remaining(), None);
}

#[test]
fn an_allocation_past_the_limit_is_refused_and_counted() {
    let mut v = with_device(Some(100));
    assert!(v.admit(64 * MIB));
    v.allocated(C, SUB, 0x10, Some(64 * MIB));
    assert!(!v.admit(64 * MIB));
    assert_eq!(v.refused(), 1);
    assert!(v.admit(36 * MIB), "exactly up to the limit fits");
    assert_eq!(v.remaining(), Some(36 * MIB));
}

#[test]
fn a_size_that_would_overflow_is_refused_not_wrapped() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(MIB));
    assert!(!v.admit(u64::MAX));
}

#[test]
fn the_charge_is_what_rm_took_not_what_was_asked() {
    let mut v = with_device(Some(100));
    assert!(v.admit(1));
    v.allocated(C, SUB, 0x10, Some(2 * MIB));
    assert_eq!(v.in_use(), 2 * MIB);
}

#[test]
fn freeing_memory_gives_it_back() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(64 * MIB));
    v.freed(C, 0x10);
    assert_eq!(v.in_use(), 0);
    assert_eq!(v.peak(), 64 * MIB);
}

/// RM frees what is under an object with it.
#[test]
fn freeing_an_ancestor_frees_the_memory_under_it() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.allocated(C, DEV, 0x11, Some(16 * MIB));
    v.freed(C, DEV);
    assert_eq!(v.in_use(), 0);
}

#[test]
fn freeing_one_branch_leaves_its_sibling() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.allocated(C, DEV, 0x11, Some(16 * MIB));
    v.freed(C, SUB);
    assert_eq!(v.in_use(), 16 * MIB);
}

#[test]
fn freeing_the_client_frees_everything_in_it() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.freed(C, C);
    assert_eq!(v.in_use(), 0);
}

#[test]
fn another_clients_memory_is_untouched() {
    let other: Client = 0xc1d0_0002;
    let mut v = with_device(Some(100));
    v.allocated(other, other, other, None);
    v.allocated(other, other, DEV, None);
    v.allocated(other, DEV, 0x10, Some(8 * MIB));
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.freed(C, DEV);
    assert_eq!(v.in_use(), 8 * MIB, "same handles, different client");
}

#[test]
fn closing_the_file_frees_its_clients() {
    let mut v = with_device(Some(100));
    v.client_opened(7, C);
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.file_closed(7);
    assert_eq!(v.in_use(), 0);
}

/// A duplicate is the same memory: charged once, and released only when the last handle to it goes.
#[test]
fn a_duplicate_costs_nothing_and_keeps_the_memory_alive() {
    let other: Client = 0xc1d0_0002;
    let mut v = with_device(Some(100));
    v.allocated(other, other, other, None);
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.duplicated(other, other, 0x20, C, 0x10);
    assert_eq!(v.in_use(), 32 * MIB);
    v.freed(C, 0x10);
    assert_eq!(v.in_use(), 32 * MIB, "the duplicate still holds it");
    v.freed(other, other);
    assert_eq!(v.in_use(), 0);
}

#[test]
fn a_duplicate_onto_its_own_handle_keeps_the_memory() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.duplicated(C, SUB, 0x10, C, 0x10);
    assert_eq!(v.in_use(), 32 * MIB);
    v.freed(C, 0x10);
    assert_eq!(v.in_use(), 0);
}

#[test]
fn a_reused_handle_does_not_leak_the_old_charge() {
    let mut v = with_device(Some(100));
    v.allocated(C, SUB, 0x10, Some(32 * MIB));
    v.allocated(C, SUB, 0x10, Some(8 * MIB));
    assert_eq!(v.in_use(), 8 * MIB);
}

#[test]
fn a_client_is_issued_only_on_the_file_it_was_made_on() {
    let mut v = Vram::new(None);
    v.client_opened(7, C);
    assert!(v.issued(7, C));
    assert!(!v.issued(8, C), "another file");
    assert!(!v.issued(7, C + 1), "another client");
}

#[test]
fn a_freed_or_closed_client_is_no_longer_issued() {
    let mut v = Vram::new(None);
    v.client_opened(7, C);
    v.client_freed(C);
    assert!(!v.issued(7, C));
    v.client_opened(7, C);
    v.file_closed(7);
    assert!(!v.issued(7, C));
}

#[test]
fn freeing_what_was_never_seen_is_harmless() {
    let mut v = with_device(Some(100));
    v.freed(C, 0xdead);
    v.freed(0xbad, 0xbad);
    v.file_closed(99);
    assert_eq!(v.in_use(), 0);
}
