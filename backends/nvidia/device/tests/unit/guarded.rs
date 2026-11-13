use super::*;

#[test]
fn the_slack_after_a_buffer_is_writable() {
    let mut b = GuardedBuf::new(100).expect("mapped");
    b.as_mut_slice()[99] = 0xab;
    assert_eq!(b.as_slice()[99], 0xab);
    // A driver that writes a little past the declared size finds memory there, as it would on the calling side.
    unsafe { b.as_mut_ptr().add(100).write(0xcd) };
    unsafe { assert_eq!(b.as_mut_ptr().add(SLACK - 1).read(), 0) };
}

#[test]
fn a_gross_overrun_still_has_a_guard_behind_it() {
    let b = GuardedBuf::new(100).expect("mapped");
    let guard = b.base as usize + b.mapped - PAGE;
    assert!(guard > b.base as usize + 100 + SLACK - PAGE);
}

/// A reused buffer must look like a fresh one of its new size: the same slack behind it, the guard right after, and no bytes left over from the call before.
#[test]
fn a_reused_buffer_has_its_slack_and_guard_where_a_fresh_one_would() {
    let mut pool = GuardPool::default();
    let mut b = pool.take(3000).expect("mapped");
    b.as_mut_slice().fill(0xee);
    let base = b.base as usize;
    pool.give(b);

    let mut b = pool.take(100).expect("reused");
    assert_eq!(b.base as usize, base, "the kept mapping is reused");
    let guard = b.base as usize + b.mapped - PAGE;
    let end = b.as_mut_ptr() as usize + 100;
    assert!(guard - end >= SLACK, "full slack behind the buffer");
    assert!(guard - end < SLACK + 16, "guard right after the slack");
    assert_eq!(b.as_mut_ptr() as usize % 16, 0);
    let tail = unsafe { std::slice::from_raw_parts(end as *const u8, guard - end) };
    assert!(
        tail.iter().all(|&x| x == 0),
        "nothing left from the last call"
    );
}

#[test]
fn a_block_bigger_than_any_kept_buffer_gets_a_fresh_one() {
    let mut pool = GuardPool::default();
    let b = pool.take(100).expect("mapped");
    pool.give(b);
    let big = pool.take(3 * PAGE).expect("mapped");
    assert!(big.capacity() >= 3 * PAGE);
    assert_eq!(pool.free.len(), 1, "the small one is still kept");
}

#[test]
fn zero_length_has_no_buffer() {
    assert!(GuardedBuf::new(0).is_none());
}
