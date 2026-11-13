use super::*;
use std::{
    os::unix::net::UnixStream,
    process::{Command, Stdio},
};

#[test]
fn disconnected_service_exits_normally_and_is_reaped() {
    let (read, write) = UnixStream::pair().expect("service channel");
    let fd: std::os::fd::OwnedFd = read.into();
    let mut child = OwnedChild(
        Command::new("cat")
            .stdin(Stdio::from(fd))
            .stdout(Stdio::null())
            .spawn()
            .expect("owned service"),
    );
    drop(write);
    child
        .stop_after_grace(Duration::from_secs(2))
        .expect("graceful shutdown");
    assert!(
        child
            .0
            .try_wait()
            .expect("reaped status")
            .expect("exited")
            .success()
    );
}

#[test]
fn stalled_service_is_killed_and_reaped_after_the_bound() {
    let mut child = OwnedChild(
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("owned stalled service"),
    );
    child
        .stop_after_grace(Duration::ZERO)
        .expect("bounded shutdown");
    assert!(
        !child
            .0
            .try_wait()
            .expect("reaped status")
            .expect("exited")
            .success()
    );
}
