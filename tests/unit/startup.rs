use super::*;

#[test]
fn stale_and_duplicate_statuses_do_not_regress_the_startup_log() {
    let output = tempfile::tempdir().expect("output");
    let mut progress = Progress::new(output.path(), false);
    progress.set(Stage::Booting);
    progress.set(Stage::Booting);
    progress.set(Stage::Graphics);
    progress.set(Stage::DisplayReady);
    progress.set(Stage::StartingApplication);
    progress.set(Stage::Complete);
    drop(progress);
    let log = fs::read_to_string(output.path().join("startup.jsonl")).expect("log");
    let stages: Vec<_> = log
        .lines()
        .map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line).expect("JSON");
            entry["stage"].as_str().expect("stage").to_owned()
        })
        .collect();
    assert_eq!(
        stages,
        ["preparing", "booting", "display-ready", "complete"]
    );
}

#[test]
fn an_unfinished_session_records_failure() {
    let output = tempfile::tempdir().expect("output");
    drop(Progress::new(output.path(), false));
    let log = fs::read_to_string(output.path().join("startup.jsonl")).expect("log");
    assert!(log.contains("\"stage\":\"failed\""));
}

#[test]
fn guest_updates_and_first_image_finish_startup_observation() {
    let output = tempfile::tempdir().expect("output");
    let frames = output.path().join("frames");
    fs::create_dir(&frames).expect("frames");
    let mut progress = Progress::new(output.path(), false);
    for stage in ["preparing-runtime", "starting-application"] {
        fs::write(
            frames.join("startup-status.json"),
            serde_json::json!({"schema":1,"stage":stage}).to_string(),
        )
        .expect("status");
        progress.last_poll = Instant::now() - Duration::from_secs(1);
        progress.poll();
    }
    assert!(progress.stage == Some(Stage::StartingApplication));
    fs::write(frames.join("vm-forward-first.rgba"), []).expect("first image");
    progress.last_poll = Instant::now() - Duration::from_secs(1);
    progress.poll();
    assert!(progress.stage == Some(Stage::DisplayReady));
    let stopped_at = progress.last_poll;
    progress.poll();
    assert_eq!(progress.last_poll, stopped_at);
    progress.set(Stage::Complete);
}
