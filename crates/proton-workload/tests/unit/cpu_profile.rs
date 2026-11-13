use super::*;
#[test]
fn worker_marker_needs_positive_ids_and_the_exact_fixture_mapping() {
    assert_eq!(marker_ids(br#"{"schema":1,"pid":7,"tid":9}"#), Some((7, 9)));
    for bytes in [
        br#"{"schema":1,"pid":0,"tid":9}"#.as_slice(),
        br#"{"schema":2,"pid":7,"tid":9}"#,
        b"{",
        br#"{"schema":1,"pid":7,"tid":-2}"#,
    ] {
        assert_eq!(marker_ids(bytes), None);
    }
    assert!(has_fixture_image(
        "1000-2000 r--p 0 00:00 1 /usr/share/legion/workload.exe"
    ));
    assert!(!has_fixture_image(
        "1000-2000 r--p 0 00:00 1 /other/workload.exe"
    ));
}

#[test]
fn user_stack_captures_only_valid_bytes_and_rejects_bad_extents() {
    let mut bytes = vec![0; 40];
    bytes.extend_from_slice(&16_u64.to_ne_bytes());
    bytes.extend_from_slice(&0x1234_u64.to_ne_bytes());
    bytes.extend_from_slice(&0x5678_u64.to_ne_bytes());
    bytes.extend_from_slice(&8_u64.to_ne_bytes());
    assert_eq!(
        decode_sample_with_stack(&bytes, true)
            .expect("bounded stack")
            .stack_words,
        [0x1234]
    );
    bytes[64..72].copy_from_slice(&17_u64.to_ne_bytes());
    assert!(decode_sample_with_stack(&bytes, true).is_err());
    bytes[40..48].copy_from_slice(&u64::from(STACK_BYTES + 8).to_ne_bytes());
    assert!(decode_sample_with_stack(&bytes, true).is_err());
    assert!(decode_sample_with_stack(&[0; 40], true).is_err());
}
#[test]
fn sampling_clock_matches_markers_and_does_not_inherit_or_include_kernel_work() {
    let attr = sampling_attribute();
    assert_eq!(attr.kind, 1);
    assert_eq!(attr.clock, libc::CLOCK_MONOTONIC);
    assert_ne!(attr.flags & (1 << 25), 0);
    assert_eq!(attr.flags & (1 << 1), 0);
    assert_eq!(attr.flags & ((1 << 5) | (1 << 6)), (1 << 5) | (1 << 6));
    assert_eq!(attr.regs, REG_MASK);
    let cycles = cycle_sampling_attribute();
    assert_eq!(cycles.kind, 0);
    assert_eq!(cycles.config, 0);
    assert_eq!(cycles.flags, attr.flags);
    assert_eq!(cycles.sample, attr.sample);
    assert_eq!(cycles.regs, attr.regs);
    assert!(cycles.period > 0);
}
#[test]
fn wrapped_records_and_short_samples_are_checked_without_reading_past_the_ring() {
    assert_eq!(ring_segments(8, 6, 4).expect("wrapped"), (6, 2));
    assert!(ring_segments(8, 0, 9).is_err());
    assert!(decode_sample(&[0; 32]).is_err());
    let mut sample = vec![0; 40];
    sample[8..16].copy_from_slice(&99_u64.to_ne_bytes());
    assert_eq!(decode_sample(&sample).expect("no-register ABI").ip, 99);
    sample[32..40].copy_from_slice(&2_u64.to_ne_bytes());
    assert!(decode_sample(&sample).is_err());
    sample.resize(40 + REG_MASK.count_ones() as usize * 8, 0);
    assert_eq!(
        decode_sample(&sample).expect("full registers").regs.len(),
        18
    );
    sample[32..40].copy_from_slice(&1_u64.to_ne_bytes());
    assert_eq!(
        decode_sample(&sample)
            .expect("32-bit transition registers")
            .abi,
        1
    );
}
