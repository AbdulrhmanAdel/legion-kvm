use super::*;

#[test]
fn keyboard_and_mouse_buttons_keep_press_release_order_without_repeats() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    for (code, value) in [(28, 1), (28, 2), (28, 0), (0x110, 1), (0x110, 0)] {
        decoder.event(1, code, value, &mut events);
    }
    assert_eq!(
        events,
        vec![
            Event::Key {
                code: 28,
                down: true
            },
            Event::Key {
                code: 28,
                down: false
            },
            Event::Button {
                code: 0x110,
                down: true
            },
            Event::Button {
                code: 0x110,
                down: false
            }
        ]
    );
}

#[test]
fn tablet_coordinates_precede_the_click_and_are_normalized_from_device_ranges() {
    let mut decoder = Decoder {
        absolute: Some([
            Axis {
                value: 0,
                min: 0,
                max: 32767,
            },
            Axis {
                value: 0,
                min: 0,
                max: 32767,
            },
        ]),
        ..Default::default()
    };
    let mut events = Vec::new();
    decoder.event(3, 0, 32767, &mut events);
    decoder.event(3, 1, -1, &mut events);
    decoder.event(1, 0x110, 1, &mut events);
    decoder.event(0, 0, 0, &mut events);
    assert_eq!(
        events,
        vec![
            Event::Absolute { x: 1.0, y: 0.0 },
            Event::Button {
                code: 0x110,
                down: true
            }
        ]
    );
    assert_eq!(
        Axis {
            value: i32::MAX,
            min: i32::MIN,
            max: i32::MAX
        }
        .normalized(),
        1.0
    );
}

#[test]
fn dropped_packets_release_keys_and_ignore_the_incomplete_frame() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    decoder.event(1, 28, 1, &mut events);
    decoder.event(0, 3, 0, &mut events);
    decoder.event(1, 30, 1, &mut events);
    decoder.event(0, 0, 0, &mut events);
    decoder.event(1, 31, 1, &mut events);
    assert_eq!(
        events,
        vec![
            Event::Key {
                code: 28,
                down: true
            },
            Event::Reset,
            Event::Key {
                code: 31,
                down: true
            }
        ]
    );
}
