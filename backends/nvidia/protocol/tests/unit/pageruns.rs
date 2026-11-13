use super::*;

const N: usize = MAX_RUNS;
static mut BUF: [u8; encoded_len(N)] = [0; encoded_len(N)];

fn fill(r: &mut [Run]) {
    for (i, e) in r.iter_mut().enumerate() {
        *e = Run {
            gpa: (i as u64 + 1) * 4096,
            len: 4096,
        };
    }
}

#[test]
fn a_table_survives_the_round_trip() {
    let mut all = [Run::default(); N];
    fill(&mut all);
    let mut buf = [0u8; encoded_len(N)];
    for n in [1, 2, 17, N] {
        let r = &all[..n];
        assert_eq!(encode(&mut buf, r), Some(encoded_len(n)));
        let parsed = Runs::parse(&buf).expect("a table just written");
        assert_eq!(parsed.len(), n);
        for (got, want) in parsed.iter().zip(r) {
            assert_eq!(got, *want);
        }
    }
}

#[test]
fn more_runs_than_the_limit_are_not_encoded() {
    let too_many = [Run::default(); N + 1];
    // SAFETY: single-threaded test, and the buffer is only used here.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
    assert_eq!(encode(&mut buf[..], &too_many), None);
}

/// A count that does not match the bytes is the shape a malformed block takes, and reading runs out of it would read whatever follows.
#[test]
fn a_count_longer_than_the_block_is_refused() {
    let mut all = [Run::default(); 2];
    fill(&mut all);
    let mut buf = [0u8; encoded_len(2)];
    encode(&mut buf, &all).unwrap();

    buf[0..4].copy_from_slice(&3u32.to_le_bytes());
    assert!(Runs::parse(&buf).is_none());

    buf[0..4].copy_from_slice(&(MAX_RUNS as u32 + 1).to_le_bytes());
    assert!(Runs::parse(&buf).is_none());
}

/// An empty table is not a registration of nothing, it is a message that says nothing, and the backend has no address to build from it.
#[test]
fn an_empty_table_is_refused() {
    assert!(Runs::parse(&[0u8; 8]).is_none());
    assert!(Runs::parse(&[]).is_none());
    assert!(Runs::parse(&[0u8; 4]).is_none());
}

/// The two sentinels have to stay distinct: a deep block read as the wrong kind is a parameter buffer read as addresses, or the reverse.
#[test]
fn the_sentinels_do_not_collide() {
    assert_ne!(PAGE_RUNS, crate::segments::SEGMENTED);
    // And neither can be a real offset into a parameter block.
    assert!(PAGE_RUNS > u16::MAX as u32);
}
