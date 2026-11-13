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
