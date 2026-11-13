use super::*;
#[test]
fn a_short_press_survives_one_poll_and_batches_keep_order() {
    let mut queue = Queue::default();
    for i in 0..42 {
        queue.push(TimedEvent {
            time_us: i,
            event: Event::Key {
                code: 30,
                down: i % 2 == 0,
            },
        });
    }
    let mut collected = Vec::new();
    let mut batch = Batch {
        width: 1920,
        height: 1080,
        ..Batch::default()
    };
    for count in [20, 20, 2] {
        queue.drain(&mut batch);
        assert_eq!(batch.events.len(), count);
        let mut bytes = [0; BYTES];
        batch.encode(&mut bytes).expect("encode");
        let decoded = Batch::decode(&bytes).expect("decode");
        collected.extend(decoded.events);
    }
    assert_eq!(
        collected.iter().map(|e| e.time_us).collect::<Vec<_>>(),
        (0..42).collect::<Vec<_>>()
    );
}
#[test]
fn overflow_releases_held_state_and_invalid_packets_cannot_create_events() {
    let mut queue = Queue::default();
    for i in 0..=QUEUE_CAPACITY {
        queue.push(TimedEvent {
            time_us: i as u64,
            event: Event::Key {
                code: 30,
                down: i % 2 == 0,
            },
        });
    }
    let mut batch = Batch {
        width: 640,
        height: 480,
        ..Batch::default()
    };
    queue.drain(&mut batch);
    assert_eq!(batch.overflows, 1);
    assert_eq!(batch.events[0].event, Event::Reset);
    let mut bytes = [0; BYTES];
    batch.encode(&mut bytes).expect("encode");
    bytes[4..8].copy_from_slice(&21u32.to_le_bytes());
    assert!(Batch::decode(&bytes).is_err());
    batch.events = vec![TimedEvent {
        time_us: 0,
        event: Event::Absolute {
            x: f64::NAN,
            y: 0.0,
        },
    }];
    assert!(batch.encode(&mut bytes).is_err());
}
#[test]
fn keymap_chunks_preserve_utf8_boundaries_and_reject_stale_generations() {
    let text = "é".repeat(2000);
    let mut bytes = [0; BYTES];
    let mut result = Vec::new();
    while result.len() < text.len() {
        keymap_request(2, result.len() as u32, &mut bytes).expect("request");
        keymap_reply(&text, 2, &mut bytes).expect("reply");
        let (_, chunk) = keymap_chunk(&bytes, 2, result.len()).expect("chunk");
        result.extend_from_slice(chunk);
    }
    assert_eq!(String::from_utf8(result).expect("complete UTF8"), text);
    keymap_request(1, 0, &mut bytes).expect("request");
    assert!(keymap_reply(&text, 2, &mut bytes).is_err());
}
