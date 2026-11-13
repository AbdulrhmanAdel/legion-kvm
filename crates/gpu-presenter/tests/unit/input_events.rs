use super::*;
fn snapshot() -> DesktopInput {
    DesktopInput {
        input: Input::new(0, 0, false, 0).expect("input"),
        closed: false,
        width: 1920,
        height: 1080,
    }
}
#[test]
fn all_keys_buttons_and_focus_loss_remain_ordered_and_window_scoped() {
    let mut capture = Capture::default();
    capture.key(30, true);
    assert!(capture.batch(snapshot()).events.is_empty());
    capture.focus(true, true);
    capture.key(30, true);
    capture.key(30, true);
    capture.key(30, false);
    capture.focus(false, true);
    capture.button(0x110, true);
    capture.button(0x110, false);
    capture.focus(true, false);
    let events = capture
        .batch(snapshot())
        .events
        .into_iter()
        .map(|e| e.event)
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        vec![
            Event::Focus {
                keyboard: true,
                focused: true
            },
            Event::Key {
                code: 30,
                down: true
            },
            Event::Key {
                code: 30,
                down: false
            },
            Event::Focus {
                keyboard: false,
                focused: true
            },
            Event::Button {
                code: 0x110,
                down: true
            },
            Event::Button {
                code: 0x110,
                down: false
            },
            Event::Focus {
                keyboard: true,
                focused: false
            }
        ]
    );
}
#[test]
fn wheel_value_and_discrete_units_are_one_event_per_axis_frame() {
    let mut capture = Capture::default();
    capture.focus(false, true);
    capture.wheel[0] = Wheel {
        value: -10.0,
        v120: -120,
        changed: true,
        ..Wheel::default()
    };
    capture.flush_wheel();
    capture.flush_wheel();
    let batch = capture.batch(snapshot());
    assert_eq!(batch.events.len(), 2);
    assert_eq!(
        batch.events[1].event,
        Event::Axis {
            axis: 0,
            value: -10.0,
            v120: -120,
            stop: false,
            source: 0
        }
    );
}
