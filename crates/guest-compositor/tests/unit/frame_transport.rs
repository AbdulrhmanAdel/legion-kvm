use super::*;
#[test]
fn final_accounting_waits_for_published_release_bookkeeping() {
    let mut reads = 0;
    let events = wait_event_accounting(3, Duration::from_secs(1), |events| {
        reads += 1;
        let acknowledged = if reads == 1 { 2_u64 } else { 3 };
        events[8..16].copy_from_slice(&acknowledged.to_le_bytes());
        Ok(())
    })
    .expect("delayed event bookkeeping completes");
    assert_eq!(reads, 2);
    assert_eq!(
        u64::from_le_bytes(events[8..16].try_into().expect("fixed packet")),
        3
    );
}

#[test]
fn final_accounting_rejects_excess_events_and_missing_completion() {
    for acknowledged in [2_u64, 4] {
        assert!(
            wait_event_accounting(3, Duration::ZERO, |events| {
                events[8..16].copy_from_slice(&acknowledged.to_le_bytes());
                Ok(())
            })
            .is_err()
        );
    }
}

#[test]
fn combined_polling_preserves_deadline_and_consumes_fresh_release_credit() {
    assert!(!poll_due(true, Duration::from_secs(1), true, false, false));
    assert!(!poll_due(
        true,
        Duration::from_micros(99),
        false,
        true,
        false
    ));
    assert!(poll_due(
        true,
        Duration::from_micros(100),
        false,
        true,
        false
    ));
    assert!(poll_due(true, Duration::ZERO, true, true, false));
    assert!(!poll_due(true, Duration::ZERO, true, true, true));
    assert!(poll_due(true, Duration::from_micros(100), true, true, true));
    assert!(poll_due(false, Duration::ZERO, true, false, true));
    assert!(poll_due(
        false,
        Duration::from_micros(100),
        false,
        false,
        true
    ));
}

#[test]
#[cfg(not(miri))] // Exercises real OS readiness; GPU event delivery needs KVM.
fn notification_readiness_is_quiet_until_data_arrives_and_detects_disconnect() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let (mut reader, mut writer) = UnixStream::pair().expect("pair");
    assert!(!notification_ready(&reader).expect("quiet"));
    writer.write_all(&[1]).expect("notify");
    assert!(notification_ready(&reader).expect("ready"));
    reader.read_exact(&mut [0]).expect("consume");
    assert!(!notification_ready(&reader).expect("quiet after consume"));
    drop(writer);
    assert!(notification_ready(&reader).is_err());
}
