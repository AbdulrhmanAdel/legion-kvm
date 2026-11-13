//! Bounded sampling for periodic animations: equal frames are inconclusive.
#[derive(Default)]
pub(crate) struct Animation {
    first: Option<u64>,
    changed: bool,
    attempts: u8,
}
pub(crate) enum Sample {
    First,
    Repeated,
    Changed,
}
impl Animation {
    pub(crate) fn first(&self) -> Option<u64> {
        self.first
    }
    pub(crate) fn needs_sample(&self) -> bool {
        !self.changed && self.attempts < 8
    }
    pub(crate) fn verified(&self) -> bool {
        self.changed
    }
    pub(crate) fn attempts(&self) -> u8 {
        self.attempts
    }
    pub(crate) fn observe(&mut self, hash: u64) -> Sample {
        let Some(first) = self.first else {
            self.first = Some(hash);
            return Sample::First;
        };
        self.attempts += 1;
        if hash != first {
            self.changed = true;
            Sample::Changed
        } else {
            Sample::Repeated
        }
    }
}
#[cfg(test)]
#[path = "../tests/unit/animation.rs"]
mod tests;
