use super::*;

#[test]
fn configurations_bound_memory_workers_and_total_runtime() {
    let mut config = Config {
        schema: 1,
        warmup_ms: 500,
        sample_ms: 1000,
        width: 1920,
        height: 1080,
        cases: vec![Case {
            name: "scene".into(),
            mode: Mode::Mixed,
            kernel: Kernel::Scene,
            workers: 8,
            working_set_kib: 512,
            work_units: 4,
            gpu_iterations: 8,
        }],
    };
    config.validate().expect("bounded mixed case");
    config.cases[0].workers = 0;
    assert!(config.validate().is_err());
    config.cases[0].workers = 64;
    config.cases[0].working_set_kib = 16384;
    assert!(config.validate().is_err());
    config.cases[0].workers = 8;
    config.cases.push(config.cases[0].clone());
    assert!(config.validate().is_err());
}
