use super::*;

#[test]
fn native_subpage_objects_still_get_complete_kvm_pages() {
    assert_eq!(page_extent(0xc70).expect("RUSD page"), 4096);
    assert_eq!(page_extent(1).expect("smallest mapping"), 4096);
    assert_eq!(page_extent(4096).expect("aligned mapping"), 4096);
    assert_eq!(page_extent(4097).expect("partial last page"), 8192);
}

#[test]
fn empty_and_overflowing_extents_are_rejected() {
    assert!(page_extent(0).is_err());
    assert!(page_extent(u64::MAX).is_err());
    assert!(page_extent(u64::MAX - 4095).is_ok());
    assert!(page_extent(u64::MAX - 4094).is_err());
}
