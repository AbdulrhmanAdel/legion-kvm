use super::*;
#[test]
fn wineserver_selection_requires_the_single_private_fixture_prefix() {
    assert!(private_prefix(
        b"HOME=/frames/home\0WINEPREFIX=/frames/compatdata/pfx\0"
    ));
    assert!(private_prefix(b"WINEPREFIX=/frames/compatdata/pfx/\0"));
    for environment in [
        b"WINEPREFIX=/other/prefix\0".as_slice(),
        b"",
        b"XWINEPREFIX=/frames/compatdata/pfx\0",
        b"WINEPREFIX=/frames/compatdata/pfx\0WINEPREFIX=/other\0",
    ] {
        assert!(!private_prefix(environment));
    }
}
#[test]
fn wakeup_samples_keep_sender_and_recipient_distinct_and_check_raw_extent() {
    let format = "field:char comm[16]; offset:8; size:16; signed:1;\nfield:pid_t pid; offset:24; size:4; signed:1;\nfield:int target_cpu; offset:32; size:4; signed:1;";
    let fields = [
        trace_field(format, "comm[16]", 16).expect("comm"),
        trace_field(format, "pid", 4).expect("pid"),
        trace_field(format, "target_cpu", 4).expect("cpu"),
    ];
    let mut record = vec![0u8; 88];
    record[16..20].copy_from_slice(&7u32.to_ne_bytes());
    record[20..24].copy_from_slice(&9u32.to_ne_bytes());
    record[32..40].copy_from_slice(&1u64.to_ne_bytes());
    record[48..52].copy_from_slice(&36u32.to_ne_bytes());
    record[60..68].copy_from_slice(b"receiver");
    record[76..80].copy_from_slice(&11u32.to_ne_bytes());
    record[84..88].copy_from_slice(&2u32.to_ne_bytes());
    let result = waking_sample(&record, 7, 9, fields, &[]).expect("wake sample");
    assert_eq!(result["tid"], 9);
    assert_eq!(result["target_tid"], 11);
    assert_eq!(result["target_comm"], "receiver");
    assert_eq!(result["target_cpu"], 2);
    assert!(waking_sample(&record, 7, 11, fields, &[]).is_err());
    assert!(waking_sample(&record[..87], 7, 9, fields, &[]).is_err());
    record[76..80].copy_from_slice(&0u32.to_ne_bytes());
    assert!(waking_sample(&record, 7, 9, fields, &[]).is_err());
    record[76..80].copy_from_slice(&11u32.to_ne_bytes());
    assert!(waking_sample(&record, 7, 9, [usize::MAX, 24, 32], &[]).is_err());
}
#[test]
fn queue_masks_follow_the_observed_cpu_and_reject_unbounded_input() {
    assert_eq!(
        cpu_list("0-2,8\n").expect("CPUs"),
        BTreeSet::from([0, 1, 2, 8])
    );
    for invalid in ["", "3-1", "0-4096", "a"] {
        assert!(cpu_list(invalid).is_err());
    }
    let words = mask(70).expect("mask");
    assert_eq!(words.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
    assert_eq!(
        words[70 / (usize::BITS as usize)],
        1usize << (70 % (usize::BITS as usize))
    );
    assert!(mask(4096).is_err());
}
#[test]
fn wakeup_control_requires_the_feature_and_rejects_ambiguous_state() {
    let mut expected = scheduler_features("TTWU_QUEUE NO_HRTICK OTHER").expect("features");
    expected.insert("TTWU_QUEUE", false);
    assert_eq!(
        scheduler_features("NO_TTWU_QUEUE NO_HRTICK OTHER").expect("features"),
        expected
    );
    for invalid in [
        "",
        "OTHER",
        "TTWU_QUEUE NO_TTWU_QUEUE",
        "TTWU_QUEUE garbage",
    ] {
        assert!(scheduler_features(invalid).is_err());
    }
}
#[test]
fn ipi_records_require_the_verified_task_and_bounded_stack_and_raw_fields() {
    let format = "field:unsigned int cpu; offset:8; size:4; signed:0;\nfield:void * callsite; offset:16; size:8; signed:0;\nfield:void * callback; offset:24; size:8; signed:0;";
    let fields = [
        trace_field(format, "cpu", 4).expect("cpu"),
        trace_field(format, "callsite", 8).expect("site"),
        trace_field(format, "callback", 8).expect("callback"),
    ];
    assert!(trace_field(format, "cpu", 8).is_err());
    assert!(trace_field(format, "missing", 4).is_err());
    let symbols = vec![
        (0x100, String::from("callback")),
        (0x200, String::from("caller")),
    ];
    let mut record = vec![0u8; 84];
    record[8..16].copy_from_slice(&0x202u64.to_ne_bytes());
    record[16..20].copy_from_slice(&7u32.to_ne_bytes());
    record[20..24].copy_from_slice(&9u32.to_ne_bytes());
    record[32..40].copy_from_slice(&1u64.to_ne_bytes());
    record[40..48].copy_from_slice(&0x204u64.to_ne_bytes());
    record[48..52].copy_from_slice(&32u32.to_ne_bytes());
    record[60..64].copy_from_slice(&3u32.to_ne_bytes());
    record[68..76].copy_from_slice(&0x200u64.to_ne_bytes());
    record[76..84].copy_from_slice(&0x100u64.to_ne_bytes());
    let sample = ipi_sample(&record, 7, 9, fields, &symbols).expect("sample");
    assert_eq!(sample["callback"], "callback+0x0");
    assert_eq!(sample["target_cpu"], 3);
    assert_eq!(sample["kernel_stack"][0], "caller+0x4");
    assert!(ipi_sample(&record, 7, 10, fields, &symbols).is_err());
    assert!(ipi_sample(&record[..83], 7, 9, fields, &symbols).is_err());
    record[32..40].copy_from_slice(&u64::MAX.to_ne_bytes());
    assert!(ipi_sample(&record, 7, 9, fields, &symbols).is_err());
}
#[test]
fn counters_retain_raw_values_and_reject_impossible_running_time() {
    let bytes = |v: [u64; 3]| {
        let mut out = [0; 24];
        for (dst, x) in out.as_chunks_mut::<8>().0.iter_mut().zip(v) {
            dst.copy_from_slice(&x.to_ne_bytes())
        }
        out
    };
    assert_eq!(
        values(bytes([42, 100, 90])).expect("counter"),
        [42, 100, 90]
    );
    assert!(values(bytes([42, 90, 100])).is_err());
    let mut stat = String::from("7 (name with ) parens)");
    for field in 0..20 {
        stat.push_str(&format!(" {field}"))
    }
    assert_eq!(identity(&stat), Some("19"));
    assert_eq!(identity("broken"), None);
}
