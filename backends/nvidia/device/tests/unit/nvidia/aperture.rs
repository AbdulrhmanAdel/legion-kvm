use super::*;

const MIB: u64 = 1 << 20;
const HVA: u64 = 8 << 30;

#[test]
fn pools_go_on_2mib_boundaries_and_reuse_freed_room() {
    let mut ap = Aperture::default();
    let a = ap.admit(1, HVA, 4096).unwrap();
    assert_eq!(a.offset, 0);
    ap.insert(1, a);
    let b = ap.admit(1, HVA + 4 * MIB, 3 * MIB).unwrap();
    assert_eq!(b.offset, 2 * MIB);
    ap.insert(2, b);
    let c = ap.admit(1, HVA + 16 * MIB, MIB).unwrap();
    assert_eq!(c.offset, 6 * MIB);
    ap.remove(1);
    assert_eq!(ap.admit(1, HVA + 32 * MIB, MIB).unwrap().offset, 0);
}

#[test]
fn a_pool_out_of_bounds_is_refused() {
    let ap = Aperture::default();
    for (addr, len) in [
        (HVA, 0),
        (HVA, 4097),
        (HVA + 1, 4096),
        (HVA, POOL_MAX_LEN + 4096),
        (HVA_MIN - 4096, 4096),
        (HVA_MAX - 4096, 8192),
        (!4095, 4096),
    ] {
        assert_eq!(
            ap.admit(1, addr, len),
            Err(libc::EINVAL),
            "{addr:#x}+{len:#x}"
        );
    }
}

#[test]
fn overlapping_and_too_many_pools_are_refused() {
    let mut ap = Aperture::default();
    ap.insert(1, ap.admit(1, HVA, 2 * MIB).unwrap());
    assert_eq!(ap.admit(1, HVA + MIB, MIB), Err(libc::EEXIST));
    for i in 1..4u32 {
        let p = ap.admit(1, HVA + i as u64 * 64 * MIB, 64 * MIB).unwrap();
        ap.insert(i + 1, p);
    }
    assert_eq!(ap.admit(1, HVA + 512 * MIB, 64 * MIB), Err(libc::ENOSPC));
}

#[test]
fn closing_a_file_takes_only_its_pools() {
    let mut ap = Aperture::default();
    ap.insert(1, ap.admit(1, HVA, MIB).unwrap());
    ap.insert(2, ap.admit(2, HVA + 8 * MIB, MIB).unwrap());
    let gone = ap.take_for_handle(1);
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].0, 1);
    assert_eq!(ap.take_all().len(), 1);
}
