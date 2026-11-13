use super::*;

#[test]
fn exchange_preserves_submission_and_can_release_that_same_frame() {
    let frame = Frame {
        resource: 7,
        frame: 19,
    };
    let mut packet = [0; EXCHANGE_BYTES];
    packet[..FRAME_BYTES].copy_from_slice(&frame.encode().expect("frame"));
    let mut releases = Releases::default();
    releases.push(frame).expect("release");
    releases.encode(&mut packet[FRAME_BYTES..]).expect("encode");
    assert_eq!(
        Frame::decode(&packet[..FRAME_BYTES]).expect("decode"),
        frame
    );
    let reply = Releases::decode(&packet[FRAME_BYTES..]).expect("decode");
    reply
        .validate(|id| (id == 7).then_some(19))
        .expect("new submission already leased");
    assert!(reply.validate(|_| None).is_err());
}

#[test]
fn malformed_size_count_padding_and_duplicate_resources_are_rejected() {
    assert!(Releases::decode(&[0; RELEASE_BYTES - 1]).is_err());
    let mut packet = [0; RELEASE_BYTES];
    packet[..4].copy_from_slice(&17_u32.to_le_bytes());
    assert!(Releases::decode(&packet).is_err());
    packet[..4].copy_from_slice(&0_u32.to_le_bytes());
    packet[4] = 1;
    assert!(Releases::decode(&packet).is_err());
    packet[4] = 0;
    packet[..4].copy_from_slice(&2_u32.to_le_bytes());
    let frame = Frame {
        resource: 7,
        frame: 1,
    }
    .encode()
    .expect("frame");
    packet[8..24].copy_from_slice(&frame);
    packet[24..40].copy_from_slice(&frame);
    assert!(Releases::decode(&packet).is_err());
    assert!(Frame::decode(&[0; FRAME_BYTES]).is_err());
}

#[test]
fn mixed_stale_and_unknown_acknowledgements_do_not_change_active_leases() {
    let active = [(7, 3), (8, 5)];
    let lookup = |id| {
        active
            .iter()
            .find(|(resource, _)| *resource == id)
            .map(|(_, frame)| *frame)
    };
    for bad in [
        Frame {
            resource: 8,
            frame: 4,
        },
        Frame {
            resource: 9,
            frame: 5,
        },
    ] {
        let mut releases = Releases::default();
        releases
            .push(Frame {
                resource: 7,
                frame: 3,
            })
            .expect("valid first release");
        releases.push(bad).expect("second release");
        assert!(releases.validate(lookup).is_err());
        assert_eq!(active, [(7, 3), (8, 5)]);
    }
}

#[test]
fn release_capacity_is_bounded_and_unused_reply_bytes_are_cleared() {
    let mut releases = Releases::default();
    for id in 1..=16 {
        releases
            .push(Frame {
                resource: id,
                frame: id + 1,
            })
            .expect("capacity");
    }
    assert!(
        releases
            .push(Frame {
                resource: 17,
                frame: 18
            })
            .is_err()
    );
    let mut packet = [255; RELEASE_BYTES];
    releases.encode(&mut packet).expect("encode full");
    assert_eq!(
        Releases::decode(&packet).expect("decode").as_slice().len(),
        16
    );
    Releases::default()
        .encode(&mut packet)
        .expect("encode empty");
    assert_eq!(packet, [0; RELEASE_BYTES]);
}
