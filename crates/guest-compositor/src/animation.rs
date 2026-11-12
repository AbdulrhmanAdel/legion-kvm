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
mod tests {
    use super::*;
    #[test]
    fn animation_cycle_returning_to_first_frame_can_still_be_verified() {
        let mut animation = Animation::default();
        animation.observe(17);
        animation.observe(17); // Initial and first delayed sample wrap a cycle.
        assert!(!animation.verified());
        assert!(animation.needs_sample());
        animation.observe(23);
        assert!(animation.verified());
        assert!(!animation.needs_sample());
        assert_eq!(animation.first(), Some(17));
    }
    #[test]
    fn static_nonuniform_image_never_passes_and_sampling_is_bounded() {
        let mut animation = Animation::default();
        animation.observe(17);
        let mut samples = 0;
        while animation.needs_sample() {
            animation.observe(17);
            samples += 1;
        }
        assert_eq!(samples, 8);
        assert!(!animation.verified());
    }
}
