use super::*;

fn entry(list: &mut Vec<u8>, index: u32, data: u32) {
    list.extend_from_slice(&index.to_le_bytes());
    list.extend_from_slice(&data.to_le_bytes());
}

#[test]
fn totals_and_frees_are_clamped_and_nothing_else_is() {
    let l = &abi::vidmem::v615_71_09::LAYOUT;
    let mut list = Vec::new();
    let gib6 = 6 << 20; // KiB
    entry(&mut list, l.index("HEAP_SIZE").unwrap(), gib6);
    entry(&mut list, l.index("RAM_SIZE").unwrap(), gib6);
    entry(&mut list, l.index("HEAP_FREE").unwrap(), gib6 - 1000);
    entry(&mut list, l.index("BAR1_SIZE").unwrap(), 256 << 10);
    entry(&mut list, l.index("HEAP_BASE_KB").unwrap(), 12345);
    entry(&mut list, l.index("HEAP_SIZE").unwrap(), 100);
    let n = clamp_fb_info(l, &mut list, 6, 2 << 20, 1 << 20);
    assert_eq!(n, 3);
    let data = |i: usize| u32_at(&list, i * 8 + 4).unwrap();
    assert_eq!(data(0), 2 << 20);
    assert_eq!(data(1), 2 << 20);
    assert_eq!(data(2), 1 << 20);
    assert_eq!(data(3), 256 << 10, "BAR1 is a window, not memory");
    assert_eq!(data(4), 12345, "a position is not a size");
    assert_eq!(data(5), 100, "already under the cap");
}

#[test]
fn a_count_past_the_list_stops_at_its_end() {
    let l = &abi::vidmem::v615_71_09::LAYOUT;
    let mut list = Vec::new();
    entry(&mut list, l.index("HEAP_SIZE").unwrap(), 6 << 20);
    assert_eq!(clamp_fb_info(l, &mut list, 1000, 1 << 20, 1 << 20), 1);
}
