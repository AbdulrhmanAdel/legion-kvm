use super::*;
use std::os::fd::FromRawFd;

fn make_fd() -> OwnedFd {
    let raw = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert!(raw >= 0);
    unsafe { OwnedFd::from_raw_fd(raw) }
}

#[test]
fn insert_get_remove() {
    let mut t = HandleTable::new();
    let h = t.insert(make_fd());
    assert!(h > 0);
    assert!(t.get_raw(h).is_ok());
    assert!(t.remove(h).is_ok());
    assert!(matches!(t.get_raw(h), Err(DeviceError::BadHandle(_))));
}

#[test]
fn handles_are_unique() {
    let mut t = HandleTable::new();
    let h1 = t.insert(make_fd());
    let h2 = t.insert(make_fd());
    assert_ne!(h1, h2);
}

#[test]
fn drain_all_empties_table() {
    let mut t = HandleTable::new();
    t.insert(make_fd());
    t.insert(make_fd());
    t.insert(make_fd());
    assert_eq!(t.len(), 3);
    t.drain_all();
    assert_eq!(t.len(), 0);
}

#[test]
fn drain_all_resets_counter() {
    let mut t = HandleTable::new();
    t.insert(make_fd()); // handle = 1
    t.insert(make_fd()); // handle = 2
    t.drain_all();
    // After drain, next handle should restart from 1
    let h = t.insert(make_fd());
    assert_eq!(h, 1, "counter should reset after drain_all");
}

#[test]
fn stale_handle_after_drain_is_rejected() {
    let mut t = HandleTable::new();
    let h = t.insert(make_fd());
    t.drain_all();
    // The old handle is gone; get_raw must fail
    assert!(matches!(t.get_raw(h), Err(DeviceError::BadHandle(_))));
}
