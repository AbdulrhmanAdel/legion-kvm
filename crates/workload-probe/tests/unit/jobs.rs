use super::*;
use crate::{Kernel, Mode};

#[test]
fn persistent_dispatch_waits_for_every_worker_and_joins_them() {
    let case = Case {
        name: "jobs".into(),
        mode: Mode::Jobs,
        kernel: Kernel::Integer,
        workers: 2,
        working_set_kib: 64,
        work_units: 1,
        gpu_iterations: 1,
    };
    let mut pool = Pool::new(&case).expect("pool");
    for _ in 0..3 {
        pool.frame().expect("all jobs completed");
    }
    let workers = pool.finish().expect("join workers");
    assert_eq!(workers.len(), 2);
    assert!(workers.iter().all(|w| w["jobs"] == 3));
    assert!(pool.finish().expect("idempotent shutdown").is_empty());
}
