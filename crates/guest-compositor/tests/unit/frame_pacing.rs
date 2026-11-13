use super::*;

#[test]
fn only_new_output_callbacks_replenish_a_consumed_credit() {
    let mut pacer = Pacer::new("callbacks").expect("paced mailbox");
    pacer.consume().expect("initial frame maps the window");
    assert!(pacer.consume().is_err());
    assert!(!pacer.callbacks_ready(0).expect("no callback yet"));
    assert!(pacer.consume().is_err());
    assert!(pacer.callbacks_ready(1).expect("first output callback"));
    pacer.consume().expect("replacement frame");
    pacer.observe(1).expect("unchanged counter");
    assert!(pacer.consume().is_err());
    pacer.observe(3).expect("batched callbacks give one credit");
    assert!(pacer.observe(2).is_err());
    pacer
        .consume()
        .expect("invalid observation preserved credit");
    assert!(pacer.consume().is_err());
}

#[test]
fn unpaced_control_keeps_unlimited_submission_credit() {
    let mut pacer = Pacer::new("unpaced").expect("unpaced control");
    for _ in 0..4 {
        pacer.consume().expect("unlimited credit");
    }
    assert!(Pacer::new("unknown").is_err());
}

#[test]
fn cancelled_first_acquisition_does_not_wait_for_a_nonexistent_output_callback() {
    let mut pacer = Pacer::new("callbacks").expect("paced mailbox");
    pacer.consume().expect("first acquisition");
    assert!(pacer.consume().is_err());
    pacer.cancel_unsubmitted();
    pacer.observe(0).expect("no desktop frame was submitted");
    pacer.consume().expect("replacement source may acquire");
    assert!(pacer.consume().is_err());
    pacer.observe(1).expect("real output callback");
    pacer.cancel_unsubmitted();
    pacer
        .consume()
        .expect("credit is coalesced with the callback");
    assert!(pacer.consume().is_err());
}
