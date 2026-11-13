use super::*;

#[test]
fn unavailable_runqueue_accounting_is_not_reported_as_zero_wait() {
    let stat = "120 0 3";
    let status = "voluntary_ctxt_switches:\t9\nnonvoluntary_ctxt_switches:\t4\n";
    let disabled = Scheduler::parse(status, stat, false);
    assert_eq!(disabled.runqueue_ns, None);
    assert_eq!(disabled.voluntary_switches, 9);
    assert_eq!(disabled.involuntary_switches, 4);
    let enabled = Scheduler::parse(status, "200 35 5", true);
    assert_eq!(enabled.runqueue_ns, Some(35));
    assert_eq!(
        enabled
            .since(&Scheduler::parse(status, "100 20 2", true))
            .runqueue_ns,
        Some(15)
    );
}
