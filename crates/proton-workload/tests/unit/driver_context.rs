use super::*;
#[test]
fn context_requires_the_exact_caller_and_register_abi() {
    let base = 0x10000000;
    let mut sample = Sample {
        ip: base + YIELD_RET,
        pid: 7,
        tid: 9,
        time_ns: 0,
        abi: 2,
        regs: vec![0; 18],
        stack_words: vec![0; 9],
    };
    sample.regs[15] = 0x1234;
    sample.regs[5] = 0x5678;
    sample.stack_words[0] = base + BACKOFF_RET;
    sample.stack_words[8] = base + OUTER_RET;
    assert_eq!(sample_context(&sample, base), Some((0x1234, true)));
    sample.stack_words[8] += 1;
    assert_eq!(sample_context(&sample, base), None);
    sample.ip = base + 0xa8b30e;
    sample.stack_words[0] = base + 0xa7f2aa;
    assert_eq!(sample_context(&sample, base), Some((0x5678, false)));
    sample.stack_words.clear();
    assert_eq!(sample_context(&sample, base), None);
    sample.abi = 1;
    assert_eq!(sample_context(&sample, base), None);
}
#[test]
fn mappings_reject_overflow_and_cross_boundary_reads() {
    let m = Mapping::parse("1000-2000 r-xp 00001000 00:01 1 /driver.so").expect("map");
    assert!(m.contains(0x1ff8, 8));
    assert!(!m.contains(0x1ffc, 8));
    assert!(!m.contains(u64::MAX - 1, 8));
    assert_eq!(m.file_offset(0x1234), Some(0x1234));
    assert!(resolve(&[m], 0x2000).is_err());
    assert!(offset(u64::MAX, 1).is_err());
    assert!(Mapping::parse("bad maps").is_none());
}
