use super::*;
use crate::Mode;

#[test]
fn dependent_load_permutation_covers_the_whole_working_set() {
    let case = Case {
        name: "memory".into(),
        mode: Mode::Throughput,
        kernel: Kernel::Memory,
        workers: 1,
        working_set_kib: 64,
        work_units: 1,
        gpu_iterations: 1,
    };
    let State::Memory { next, .. } = State::new(&case, 0).expect("state") else {
        unreachable!()
    };
    let mut seen = vec![false; next.len()];
    let mut at = 0;
    for _ in 0..next.len() {
        assert!(!seen[at]);
        seen[at] = true;
        at = next[at] as usize;
    }
    assert_eq!(at, 0);
    assert!(seen.into_iter().all(|x| x));
    assert_eq!(
        known_answer(&case, 0).expect("answer"),
        known_answer(&case, 0).expect("repeat")
    );
}
