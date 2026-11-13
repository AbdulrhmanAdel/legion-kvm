use super::*;
#[test]
fn input_snapshot_tracks_coordinates_and_actual_key_state_without_repeats() {
    let mut state = State {
        input: Some(DesktopInput {
            input: Input::new(0, 0, false, 0).expect("initial input"),
            closed: false,
            width: 1280,
            height: 720,
        }),
        ..State::default()
    };
    state.pointer_position(640.0, 360.0);
    assert_eq!(state.input.expect("input").input.x(), 32767);
    state.space(true);
    let pressed = state.input.expect("input").input;
    state.space(true);
    assert_eq!(state.input.expect("input").input, pressed);
    state.space(false);
    assert!(!state.input.expect("input").input.space());
    assert_eq!(
        state.input.expect("input").input.generation(),
        pressed.generation() + 1
    );
    state.pointer_position(-2.0, 2000.0);
    assert_eq!(
        (
            state.input.expect("input").input.x(),
            state.input.expect("input").input.y()
        ),
        (0, 65535)
    );
}
#[test]
fn lease_requires_consumer_release_and_gpu_completion() {
    let mut lease = Lease::Idle;
    lease.begin(1).expect("initial frame");
    assert_eq!(lease.complete(true), None);
    assert!(lease.begin(2).is_err());
    lease.consumer_release();
    assert_eq!(lease.complete(false), None);
    assert!(lease.begin(2).is_err());
    assert_eq!(lease.complete(true), Some(1));
    assert_eq!(lease.complete(true), None);
    lease.begin(2).expect("completed buffer can be reused");
    lease.consumer_release();
    lease.consumer_release();
    assert_eq!(lease.complete(true), Some(2));
}
#[test]
fn idle_or_duplicate_notifications_never_create_releases() {
    let mut lease = Lease::Idle;
    lease.consumer_release();
    assert_eq!(lease.complete(true), None);
    assert!(lease.begin(0).is_err());
}
#[test]
#[cfg(not(miri))]
fn write_readiness_is_not_reported_while_a_completion_fd_is_blocked() {
    use rustix::pipe::{PipeFlags, pipe_with};
    let (reader, writer) =
        pipe_with(PipeFlags::NONBLOCK | PipeFlags::CLOEXEC).expect("completion pipe");
    let bytes = [0_u8; 4096];
    while rustix::io::write(&writer, &bytes).is_ok() {}
    let planes = [Plane {
        fd: writer,
        offset: 0,
        stride: 4,
    }];
    assert!(!planes_complete(&planes).expect("not ready"));
    let mut bytes = [0_u8; 4096];
    rustix::io::read(&reader, &mut bytes).expect("complete pending use");
    assert!(planes_complete(&planes).expect("ready after completion"));
}
