use super::*;
use std::io::{BufRead, BufReader};

fn group(script: &str) -> (ForwardChild, u32) {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .process_group(0)
        .stdout(Stdio::piped());
    let mut child = ForwardChild {
        child: OwnedChild(command.spawn().expect("owned process group")),
        reaped: false,
    };
    let mut line = String::new();
    BufReader::new(child.child.0.stdout.take().expect("PID pipe"))
        .read_line(&mut line)
        .expect("descendant PID");
    (child, line.trim().parse().expect("PID number"))
}

fn assert_terminated(pid: u32) {
    let start = Instant::now();
    loop {
        let running = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|text| {
                let state = text.rsplit_once(") ").expect("process stat").1.as_bytes()[0];
                state != b'Z' && state != b'X'
            });
        if !running {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "owned descendant remained running"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn forward_group_cleans_descendants_when_leader_exits() {
    let mut unrelated = OwnedChild(
        Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("unrelated sentinel"),
    );
    let (mut child, descendant) = group("/bin/sleep 30 & printf '%s\\n' \"$!\"; exit 0");
    assert!(
        child
            .wait_until(Duration::from_secs(2))
            .expect("leader exit")
            .success()
    );
    assert_terminated(descendant);
    assert!(unrelated.0.try_wait().expect("sentinel status").is_none());
}

#[test]
fn forward_group_timeout_drop_cleans_descendants_and_reaps_leader() {
    let (mut child, descendant) = group("/bin/sleep 30 & printf '%s\\n' \"$!\"; wait");
    let leader = child.child.0.id();
    assert!(child.wait_until(Duration::from_millis(20)).is_err());
    drop(child);
    assert_terminated(descendant);
    assert!(!Path::new(&format!("/proc/{leader}")).exists());
}
