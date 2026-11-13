use super::*;

fn small_cfg() -> ZoneConfig {
    ZoneConfig {
        uc_size: 4096 * 2,
        wc_size: 4096 * 4,
        wb_size: 4096 * 2,
    }
}

fn small_alloc() -> ShmAllocator {
    ShmAllocator::new(small_cfg())
}

#[test]
fn memfd_is_valid() {
    let a = small_alloc();
    assert!(a.memfd_raw() >= 0);
    assert!(!a.base_ptr().is_null());
    assert_eq!(a.total_size(), 4096 * 8);
}

#[test]
fn zones_dont_overlap() {
    let a = small_alloc();
    assert_eq!(a.uc.base + a.uc.size, a.wc.base);
    assert_eq!(a.wc.base + a.wc.size, a.wb.base);
}

#[test]
fn alloc_correct_zone() {
    let mut a = small_alloc();
    let uc = a.alloc(100, PgprotKind::Uncached).unwrap();
    let wc = a.alloc(100, PgprotKind::WriteCombine).unwrap();
    let wb = a.alloc(100, PgprotKind::WriteBack).unwrap();

    assert_eq!(uc.offset, a.uc.base);
    assert_eq!(wc.offset, a.wc.base);
    assert_eq!(wb.offset, a.wb.base);
}

#[test]
fn alloc_respects_page_alignment() {
    let mut a = small_alloc();
    let r1 = a.alloc(1, PgprotKind::WriteBack).unwrap();
    let r2 = a.alloc(1, PgprotKind::WriteBack).unwrap();
    assert_eq!(r2.offset - r1.offset, 4096);
}

#[test]
fn zone_full_returns_error() {
    let mut a = small_alloc();
    a.alloc(4096 * 2, PgprotKind::Uncached).unwrap();
    assert!(a.alloc(1, PgprotKind::Uncached).is_err());
    assert!(a.alloc(4096, PgprotKind::WriteCombine).is_ok());
}

#[test]
fn set_base_ptr_changes_target() {
    let mut a = small_alloc();
    let original = a.base_ptr();
    let fake_ptr = 0xDEAD_0000 as *mut u8;
    a.set_base_ptr(fake_ptr);
    assert_eq!(a.base_ptr(), fake_ptr);
    assert_ne!(a.base_ptr(), original);
}

#[test]
fn map_host_fd_with_memfd_fallback() {
    // Tests using the default memfd-backed base_ptr (no set_base_ptr call)
    let mut a = small_alloc();
    let region = a.alloc(4096, PgprotKind::WriteCombine).unwrap();

    let name = CString::new("test-host-fd").unwrap();
    let host_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    assert!(host_fd >= 0);
    unsafe {
        libc::ftruncate(host_fd, 4096);
        let tmp = libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            host_fd,
            0,
        );
        assert_ne!(tmp, libc::MAP_FAILED);
        *(tmp as *mut u8) = 0x42;
        libc::munmap(tmp, 4096);
    }

    a.map_host_fd(region.offset, 4096, host_fd).unwrap();

    unsafe {
        let val = *a.base_ptr().add(region.offset as usize);
        assert_eq!(val, 0x42);
    }

    unsafe { libc::close(host_fd) };
}
