//! Interactive play has a startup deadline, rather than a session time limit.
use anyhow::{Context, Result, ensure};
use std::time::Duration;

/// First-run prefix setup and shader initialization need their own budget.
pub(crate) fn startup_timeout() -> Result<Duration> {
    let value = match std::env::var("LEGION_STARTUP_TIMEOUT_SECONDS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error).context("invalid LEGION_STARTUP_TIMEOUT_SECONDS"),
    };
    parse_startup_timeout(value.as_deref())
}

fn parse_startup_timeout(value: Option<&str>) -> Result<Duration> {
    let seconds = value
        .unwrap_or("180")
        .parse::<u64>()
        .context("LEGION_STARTUP_TIMEOUT_SECONDS must be an integer from 30 to 900")?;
    ensure!(
        (30..=900).contains(&seconds),
        "LEGION_STARTUP_TIMEOUT_SECONDS must be 30..900"
    );
    Ok(Duration::from_secs(seconds))
}

/// This survives VM teardown, including a guest subprocess that never exits.
pub(crate) fn timeout_detail(frames: &std::path::Path) -> String {
    let stage = std::fs::read(frames.join("guest-stage.json"))
        .ok()
        .filter(|bytes| bytes.len() <= 4096)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["stage"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "guest initialization".into());
    format!(
        "startup timed out during {stage}; diagnostics: {}",
        frames.display()
    )
}

pub(crate) enum Deadline {
    Bounded(Duration),
    InteractiveStartup(Duration),
}

impl Deadline {
    pub(crate) fn expired(&self, elapsed: Duration, rendering_started: bool) -> bool {
        match self {
            Self::Bounded(limit) => elapsed >= *limit,
            Self::InteractiveStartup(limit) => !rendering_started && elapsed >= *limit,
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/session.rs"]
mod tests;
