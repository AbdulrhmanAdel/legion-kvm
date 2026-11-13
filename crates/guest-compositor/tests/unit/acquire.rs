use super::*;
use std::cell::Cell;
use std::rc::Rc;

struct Fence {
    ready: Rc<Cell<bool>>,
    dropped: Rc<Cell<u32>>,
}
impl Drop for Fence {
    fn drop(&mut self) {
        self.dropped.set(self.dropped.get() + 1);
    }
}

#[test]
fn pending_fence_keeps_resource_alive_and_preserves_submission_order() {
    let ready = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(0));
    let mut queue = Queue::new(2);
    queue
        .push(
            7,
            1,
            Fence {
                ready: ready.clone(),
                dropped: dropped.clone(),
            },
        )
        .expect("first");
    queue
        .push(
            9,
            2,
            Fence {
                ready: Rc::new(Cell::new(true)),
                dropped: dropped.clone(),
            },
        )
        .expect("second");
    assert!(
        queue
            .pop_ready(|f| Ok(f.ready.get()))
            .expect("poll")
            .is_none()
    );
    assert_eq!(dropped.get(), 0);
    assert!(queue.contains(7));
    assert!(queue.is_full());
    ready.set(true);
    let first = queue
        .pop_ready(|f| Ok(f.ready.get()))
        .expect("poll")
        .expect("ready");
    assert_eq!((first.resource, first.frame), (7, 1));
    drop(first);
    assert_eq!(dropped.get(), 1);
    let second = queue
        .pop_ready(|f| Ok(f.ready.get()))
        .expect("poll")
        .expect("ready");
    assert_eq!((second.resource, second.frame), (9, 2));
    drop(second);
    assert_eq!(dropped.get(), 2);
    assert!(queue.is_empty());
}

#[test]
fn poll_error_and_duplicate_or_full_submissions_preserve_existing_leases() {
    let mut queue = Queue::new(2);
    queue.push(7, 11, ()).expect("first");
    assert!(queue.push(7, 12, ()).is_err());
    assert!(queue.push(8, 0, ()).is_err());
    queue.push(8, 12, ()).expect("second");
    assert!(queue.push(9, 13, ()).is_err());
    assert!(queue.pop_ready(|_| anyhow::bail!("GPU failure")).is_err());
    let first = queue
        .pop_ready(|_| Ok(true))
        .expect("poll")
        .expect("retained");
    assert_eq!((first.resource, first.frame), (7, 11));
    assert!(queue.contains(8));
}
