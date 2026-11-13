use super::fake::{FakeRam, mark};
use super::*;

fn ram() -> FakeRam {
    // Two regions with a hole between them, as a real guest has.
    FakeRam::new(&[(0, 16 * PAGE), (1024 * PAGE, 16 * PAGE)])
}

fn page_at(s: &Stitched, i: u64) -> u8 {
    // SAFETY: inside the stitched span, which is mapped read-write.
    unsafe { *((s.addr() + i * PAGE) as *const u8) }
}

/// The property the whole thing exists for: the host address RM is given reads back the guest's own pages, in the order the guest asked for, however they are scattered.
#[test]
fn a_stitched_span_is_the_guest_s_pages_in_order() {
    let ram = ram();
    ram.fill();
    // Out of order, from both regions, with a hole skipped.
    let runs = [
        Run {
            gpa: 8 * PAGE,
            len: 2 * PAGE,
        },
        Run {
            gpa: 1024 * PAGE,
            len: PAGE,
        },
        Run { gpa: 0, len: PAGE },
    ];
    let s = stitch(&ram, &runs, 4 * PAGE).expect("these are all guest pages");

    assert_eq!(page_at(&s, 0), mark(8 * PAGE));
    assert_eq!(page_at(&s, 1), mark(9 * PAGE));
    assert_eq!(page_at(&s, 2), mark(1024 * PAGE));
    assert_eq!(page_at(&s, 3), mark(0));
}

/// And it is an alias, not a copy: a write through the span is a write to the guest's page, which is what makes the GPU and the guest agree.
#[test]
fn writing_through_the_span_writes_the_guest_s_page() {
    let ram = ram();
    ram.fill();
    let s = stitch(
        &ram,
        &[Run {
            gpa: 4 * PAGE,
            len: PAGE,
        }],
        PAGE,
    )
    .unwrap();
    // SAFETY: inside the span.
    unsafe { *(s.addr() as *mut u8) = 0x5c };

    let again = stitch(
        &ram,
        &[Run {
            gpa: 4 * PAGE,
            len: PAGE,
        }],
        PAGE,
    )
    .unwrap();
    assert_eq!(page_at(&again, 0), 0x5c, "the two spans must see one page");
}

/// An address that is not guest RAM is the attack this is for.
#[test]
fn an_address_outside_guest_ram_is_refused() {
    let ram = ram();
    // Past the first region, past the second, and in the hole between them.
    for gpa in [100 * PAGE, 1040 * PAGE, (1 << 40) * PAGE] {
        let e = stitch(&ram, &[Run { gpa, len: PAGE }], PAGE).unwrap_err();
        assert!(e.contains("not this guest's memory"), "{gpa:#x}: {e}");
    }
}

/// A run that starts in a region and runs off the end of it would take whatever follows.
#[test]
fn a_run_that_leaves_its_region_is_refused() {
    let ram = ram();
    let e = stitch(
        &ram,
        &[Run {
            gpa: 15 * PAGE,
            len: 2 * PAGE,
        }],
        2 * PAGE,
    )
    .unwrap_err();
    assert!(e.contains("in one region"), "{e}");
}

/// The runs have to account for the whole registration and no more.
#[test]
fn the_pages_must_add_up_to_what_is_being_registered() {
    let ram = ram();
    for (runs, want) in [
        (vec![Run { gpa: 0, len: PAGE }], 2 * PAGE),
        (
            vec![Run {
                gpa: 0,
                len: 2 * PAGE,
            }],
            PAGE,
        ),
    ] {
        let e = stitch(&ram, &runs, want).unwrap_err();
        assert!(e.contains("cover"), "{e}");
    }
}

/// A partial page cannot be mapped, and rounding one up would hand RM the neighbouring page -- which may be anyone's.
#[test]
fn a_run_that_is_not_whole_pages_is_refused() {
    let ram = ram();
    let e = stitch(&ram, &[Run { gpa: 64, len: PAGE }], PAGE).unwrap_err();
    assert!(e.contains("whole pages"), "{e}");
    let e = stitch(&ram, &[Run { gpa: 0, len: 64 }], 64).unwrap_err();
    assert!(e.contains("whole pages"), "{e}");
}

#[test]
fn an_unreasonable_registration_is_refused_before_anything_is_mapped() {
    let ram = ram();
    assert!(stitch(&ram, &[], 0).unwrap_err().contains("no pages"));
    assert!(
        stitch(&ram, &[Run { gpa: 0, len: PAGE }], MAX_BYTES + PAGE)
            .unwrap_err()
            .contains("limit")
    );
    let many: Vec<_> = (0..MAX_RUNS + 1)
        .map(|i| Run {
            gpa: i as u64 * PAGE,
            len: PAGE,
        })
        .collect();
    assert!(
        stitch(&ram, &many, (MAX_RUNS as u64 + 1) * PAGE)
            .unwrap_err()
            .contains("runs")
    );
}

/// A run that wraps the address space would pass a naive end-of-run check.
#[test]
fn a_run_that_wraps_is_refused() {
    let ram = ram();
    let e = stitch(
        &ram,
        &[Run {
            gpa: u64::MAX - PAGE + 1,
            len: 2 * PAGE,
        }],
        2 * PAGE,
    )
    .unwrap_err();
    assert!(e.contains("wraps"), "{e}");
}
