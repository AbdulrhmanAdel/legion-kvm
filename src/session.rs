//! Interactive play has a startup deadline, rather than a session time limit.
use std::time::Duration;

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
