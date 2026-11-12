use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Limit crash outputs for this diagnostic process and its children only.
pub struct CoreDumpLimit(rustix::process::Rlimit);

impl CoreDumpLimit {
    pub fn disabled() -> Result<Self> {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let previous = getrlimit(Resource::Core);
        setrlimit(
            Resource::Core,
            Rlimit {
                current: Some(0),
                maximum: previous.maximum,
            },
        )
        .context("disable core dumps for this diagnostic's children")?;
        Ok(Self(previous))
    }
}

impl Drop for CoreDumpLimit {
    fn drop(&mut self) {
        let _ = rustix::process::setrlimit(rustix::process::Resource::Core, self.0);
    }
}

/// Own and reap only the process spawned for this operation.
pub struct OwnedChild(pub Child);

impl OwnedChild {
    pub fn wait_until(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().context("poll child process")? {
                return Ok(status);
            }
            if start.elapsed() >= timeout {
                bail!(
                    "child process exceeded {} second timeout",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.0.try_wait()?.is_none() {
            self.0.kill().context("stop owned child process")?;
        }
        self.0.wait().context("reap owned child process")?;
        Ok(())
    }

    /// After disconnecting a service's client, allow its owned resources and final counters to drain.
    pub fn stop_after_grace(&mut self, grace: Duration) -> Result<()> {
        let start = Instant::now();
        while self
            .0
            .try_wait()
            .context("poll disconnected service")?
            .is_none()
            && start.elapsed() < grace
        {
            std::thread::sleep(
                Duration::from_millis(10).min(grace.saturating_sub(start.elapsed())),
            );
        }
        self.stop()
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
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
}
