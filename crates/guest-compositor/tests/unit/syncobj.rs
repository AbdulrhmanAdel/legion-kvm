use super::*;
#[test]
fn an_unsignaled_notification_does_not_release_a_commit() {
    let event = rustix::event::eventfd(
        0,
        rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
    )
    .expect("eventfd");
    assert!(!event_ready(&event).expect("pending notification"));
    rustix::io::write(&event, &1_u64.to_ne_bytes()).expect("signal event");
    assert!(event_ready(&event).expect("completed notification"));
    let mut data = [0_u8; 8];
    rustix::io::read(&event, &mut data).expect("consume event");
    assert!(!event_ready(&event).expect("consumed notification"));
}
#[test]
fn a_closed_dependency_is_an_error_not_gpu_completion() {
    let (read, write) = std::os::unix::net::UnixStream::pair().expect("notification pair");
    drop(write);
    assert!(event_ready(&read).is_err());
}
