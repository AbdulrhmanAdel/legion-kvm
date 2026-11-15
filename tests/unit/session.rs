use super::*;

#[test]
fn interactive_startup_has_an_independent_validated_budget() {
    let default = parse_startup_timeout(None).expect("default deadline");
    assert_eq!(default, Duration::from_secs(180));
    let deadline = Deadline::InteractiveStartup(default);
    assert!(!deadline.expired(Duration::from_secs(32), false));
    assert!(deadline.expired(Duration::from_secs(180), false));
    assert!(!deadline.expired(Duration::from_secs(900), true));
    for value in ["", "garbage", "0", "29", "901", "-1"] {
        assert!(parse_startup_timeout(Some(value)).is_err(), "{value}");
    }
    assert_eq!(
        parse_startup_timeout(Some("300")).expect("override"),
        Duration::from_secs(300)
    );
}

#[test]
fn timeout_preserves_the_last_guest_stage_and_log_location() {
    let frames = tempfile::tempdir().expect("frames");
    assert!(timeout_detail(frames.path()).contains("guest initialization"));
    std::fs::write(
        frames.path().join("guest-stage.json"),
        r#"{"schema":1,"stage":"vulkaninfo"}"#,
    )
    .expect("stage");
    let message = timeout_detail(frames.path());
    assert!(message.contains("vulkaninfo"));
    assert!(message.contains(frames.path().to_str().expect("path")));
    std::fs::write(frames.path().join("guest-stage.json"), "incomplete").expect("partial write");
    assert!(timeout_detail(frames.path()).contains("guest initialization"));
}

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
