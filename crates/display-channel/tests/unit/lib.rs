use super::*;

#[test]
fn pixel_copy_preserves_tails_and_supports_unaligned_fallback() {
    #[repr(align(64))]
    struct Aligned([u8; 384]);
    let src = Aligned(std::array::from_fn(|i| (i % 251) as u8));
    for (source_offset, destination_offset) in [(0, 0), (1, 2)] {
        for len in [0, 1, 3, 63, 64, 65, 129, 256, 319] {
            let mut dst = Aligned([0xfe; 384]);
            // SAFETY: separate aligned arrays contain the complete ranges, including offsets and tails.
            unsafe {
                copy_pixels(
                    src.0.as_ptr().add(source_offset),
                    dst.0.as_mut_ptr().add(destination_offset),
                    len,
                    true,
                );
            }
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            // SAFETY: x86-64 supports SFENCE.
            unsafe {
                std::arch::asm!("sfence", options(nostack, preserves_flags));
            }
            assert_eq!(
                &dst.0[destination_offset..destination_offset + len],
                &src.0[source_offset..source_offset + len]
            );
            assert!(dst.0[..destination_offset].iter().all(|b| *b == 0xfe));
            assert!(dst.0[destination_offset + len..].iter().all(|b| *b == 0xfe));
        }
    }
}

fn channel() -> Channel {
    let layout = std::alloc::Layout::from_size_align(8192, 64).expect("test layout");
    // SAFETY: layout is nonzero and valid; ownership is retained by guard.
    let base = NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).expect("test allocation");
    let c = Channel {
        base,
        // SAFETY: this test allocation contains the header and two slots.
        pixels: unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER_BYTES)) },
        len: 8192,
        width: 8,
        height: 8,
        file: None,
        write_combining: false,
        _mapping: Mapping::Test {
            _allocation: TestAllocation { base, layout },
        },
    };
    c.initialize();
    c
}
fn meta(number: u64) -> FrameMeta {
    let input = Input::new(20, 40, true, 1).expect("input");
    FrameMeta {
        kind: FrameKind::InputColor,
        number,
        input,
        rgba: input.rgba(number),
        gpu_ns: 17,
        copy_ns: 0,
    }
}
#[test]
fn application_frames_preserve_kind_and_arbitrary_pixels() {
    let c = channel();
    let mut producer = c.producer().expect("producer");
    let mut consumer = c.consumer().expect("consumer");
    let pixels = std::array::from_fn::<u8, 256, _>(|i| (i % 251) as u8);
    let mut metadata = meta(1);
    metadata.kind = FrameKind::Application;
    metadata.rgba = [0; 4];
    assert!(producer.publish(&pixels, metadata).expect("publish"));
    let frame = consumer.read().expect("application frame");
    assert_eq!(frame.meta.kind, FrameKind::Application);
    assert_eq!(frame.pixels(), &pixels);
}

#[test]
fn full_ring_never_overwrites_an_unread_or_borrowed_frame() {
    let c = channel();
    let mut p = c.producer().expect("producer");
    let mut r = c.consumer().expect("consumer");
    assert!(p.publish(&[1; 256], meta(1)).expect("first"));
    let frame = r.read().expect("first frame");
    assert_eq!(frame.pixels(), &[1; 256]);
    assert!(p.publish(&[2; 256], meta(2)).expect("second"));
    assert!(!p.publish(&[3; 256], meta(3)).expect("full ring"));
    assert_eq!(frame.pixels(), &[1; 256]);
    drop(frame);
    assert!(p.publish(&[3; 256], meta(3)).expect("reuse"));
}
#[test]
fn consumer_selects_newest_frame_and_discards_stale_ready_slot() {
    let c = channel();
    let mut p = c.producer().expect("producer");
    let mut r = c.consumer().expect("consumer");
    assert!(p.publish(&[1; 256], meta(1)).expect("first"));
    assert!(p.publish(&[2; 256], meta(2)).expect("second"));
    let frame = r.read().expect("latest");
    assert_eq!(frame.meta.number, 2);
    assert!(
        p.publish(&[3; 256], meta(3))
            .expect("discarded slot reusable")
    );
    assert_eq!(frame.pixels(), &[2; 256]);
}
#[test]
fn roles_and_lengths_are_checked() {
    let c = channel();
    let mut p = c.producer().expect("producer");
    assert!(c.producer().is_err());
    let _r = c.consumer().expect("consumer");
    assert!(c.consumer().is_err());
    assert!(p.publish(&[0; 255], meta(1)).is_err());
    assert!(image_bytes(u32::MAX, 2).is_err());
    assert!(Input::new(0, 0, false, 0x100_0000).is_err());
}
#[test]
fn input_roundtrip_is_atomic_and_preserves_all_fields() {
    let c = channel();
    let mut r = c.consumer().expect("consumer");
    let input = Input::new(0x1234, 0xabcd, true, 123).expect("input");
    r.set_input(input);
    assert_eq!(c.input(), input);
    assert_eq!(c.input().rgba(10), [0x12, 0xab, 224, 255]);
}

pub(super) struct TestAllocation {
    base: NonNull<u8>,
    layout: std::alloc::Layout,
}
impl Drop for TestAllocation {
    fn drop(&mut self) {
        // SAFETY: this allocation was created with exactly this layout and remains owned until all channel endpoint borrows have ended.
        unsafe { std::alloc::dealloc(self.base.as_ptr(), self.layout) };
    }
}
