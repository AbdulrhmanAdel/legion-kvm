use super::forwarding_timeout;
use std::time::Duration;

#[test]
fn application_keeps_running_after_an_hour_while_fixture_limits_remain() {
    let elapsed = Duration::from_secs(3601);
    assert!(forwarding_timeout(true, false, 15).is_none_or(|limit| elapsed < limit));
    assert_eq!(
        forwarding_timeout(false, true, 15),
        Some(Duration::from_secs(75))
    );
    assert_eq!(
        forwarding_timeout(false, false, 15),
        Some(Duration::from_secs(55))
    );
}
