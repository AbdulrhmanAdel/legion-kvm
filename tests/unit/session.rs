use super::*;

#[test]
fn interactive_play_survives_its_old_limit_but_startup_remains_bounded() {
    let deadline = Deadline::InteractiveStartup(Duration::from_secs(60));
    assert!(!deadline.expired(Duration::from_secs(59), false));
    assert!(deadline.expired(Duration::from_secs(60), false));
    assert!(!deadline.expired(Duration::from_secs(3601), true));
    assert!(!deadline.expired(Duration::from_secs(86400), true));
}

#[test]
fn benchmark_deadline_still_applies_after_rendering_starts() {
    let deadline = Deadline::Bounded(Duration::from_secs(60));
    assert!(!deadline.expired(Duration::from_secs(59), true));
    assert!(deadline.expired(Duration::from_secs(60), true));
    assert!(deadline.expired(Duration::from_secs(3601), true));
}
