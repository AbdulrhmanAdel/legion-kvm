#[cfg(not(miri))]
mod gpu;

#[cfg(not(miri))]
use anyhow::{Context, Result, ensure};
#[cfg(not(miri))]
use legion_workload_probe::{
    Case, Config, Mode, jobs,
    stats::{self, Scheduler},
};
#[cfg(not(miri))]
use serde_json::{Value, json};
#[cfg(not(miri))]
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

#[cfg(not(miri))]
fn main() {
    if let Err(error) = run() {
        println!("{}", json!({"passed":false,"error":format!("{error:#}")}));
        std::process::exit(1);
    }
}

#[cfg(miri)]
fn main() {}

#[cfg(not(miri))]
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2,
        "expected <configuration.json> <new-result.json>"
    );
    let path = Path::new(&args[1]);
    ensure!(!path.exists(), "workload result must not exist");
    let config: Config = serde_json::from_slice(&fs::read(&args[0])?)?;
    config.validate()?;
    let topology = legion_cpu_topology::Topology::detect()?;
    for case in &config.cases {
        ensure!(
            case.workers <= topology.logical_cpus as usize,
            "worker count exceeds online CPUs"
        );
    }
    let mut progress = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path.with_extension("progress.jsonl"))?;
    let mut results = Vec::with_capacity(config.cases.len());
    for case in &config.cases {
        let mut result = if case.mode == Mode::Throughput {
            jobs::throughput(
                case,
                Duration::from_millis(config.warmup_ms),
                Duration::from_millis(config.sample_ms),
            )?
        } else {
            frames(&config, case)?
        };
        result["case"] = serde_json::to_value(case)?;
        writeln!(
            progress,
            "{}",
            json!({"name":case.name,"jobs_per_second":result.get("jobs_per_second"),"frames_per_second":result.get("frames_per_second"),"cpu_process_seconds":result.get("cpu_process_seconds")})
        )?;
        progress.flush()?;
        results.push(result);
    }
    let gpu_cases = config
        .cases
        .iter()
        .filter(|c| matches!(c.mode, Mode::Gpu | Mode::Mixed))
        .count();
    let gpu_verified = results
        .iter()
        .filter(|r| r.get("gpu").is_some())
        .all(|r| r["gpu_known_answer_verified"] == true)
        && gpu_cases > 0;
    let status = fs::read_to_string("/proc/self/status")?;
    let affinity = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim);
    let report = json!({"schema":1,"passed":true,"offscreen":true,"presentation_included":false,"configuration":config,"topology":topology,"affinity":affinity,"build_features":{"avx2":cfg!(target_feature="avx2"),"fma":cfg!(target_feature="fma"),"avx512f":cfg!(target_feature="avx512f")},"runtime_features":{"avx2":std::is_x86_feature_detected!("avx2"),"avx512f":std::is_x86_feature_detected!("avx512f")},"clocksource":fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource").ok().map(|s|s.trim().to_owned()),"gpu_verified":gpu_verified,"cases":results});
    serde_json::to_writer_pretty(File::create_new(path)?, &report)?;
    println!(
        "{}",
        json!({"schema":1,"passed":true,"gpu_verified":gpu_verified,"case_count":config.cases.len(),"result":path})
    );
    Ok(())
}

#[cfg(not(miri))]
fn frames(config: &Config, case: &Case) -> Result<Value> {
    let use_gpu = matches!(case.mode, Mode::Gpu | Mode::Mixed);
    let mut gpu = use_gpu
        .then(|| gpu::Gpu::new(config.width, config.height, case.gpu_iterations))
        .transpose()?;
    let first_hash = gpu.as_mut().map(gpu::Gpu::verify).transpose()?;
    let mut pool = jobs::Pool::new(case)?;
    let warmup = Instant::now();
    let mut checksum = 0_u64;
    while warmup.elapsed() < Duration::from_millis(config.warmup_ms) {
        if let Some(gpu) = &mut gpu {
            gpu.acquire()?;
        }
        checksum ^= pool.frame()?;
        if let Some(gpu) = &mut gpu {
            gpu.submit(checksum as u32)?;
        }
    }
    if let Some(gpu) = &mut gpu {
        gpu.drain()?;
    }
    // Nonzero initialization also faults in the recording pages before timing.
    const MAX_SAMPLES: usize = 1_000_000;
    let mut intervals = vec![u64::MAX; MAX_SAMPLES];
    let mut cpu_stages = vec![u64::MAX; MAX_SAMPLES];
    let mut submit_stages = vec![u64::MAX; MAX_SAMPLES];
    let mut wait_stages = vec![u64::MAX; MAX_SAMPLES];
    let mut gpu_stages = vec![u64::MAX; MAX_SAMPLES];
    let mut gpu_samples = 0;
    let memory_pages = stats::memory_pages();
    let cpu_start = stats::cpu_time(false);
    let schedule = Scheduler::current();
    let start = Instant::now();
    let mut previous = start;
    let mut count = 0_u64;
    while start.elapsed() < Duration::from_millis(config.sample_ms) {
        ensure!(
            count < MAX_SAMPLES as u64,
            "workload exceeded frame sample limit"
        );
        let index = count as usize;
        let before = Instant::now();
        if let Some(gpu) = &mut gpu
            && let Some(ns) = gpu.acquire()?
        {
            gpu_stages[gpu_samples] = ns;
            gpu_samples += 1;
        }
        wait_stages[index] = before.elapsed().as_nanos() as u64;
        let before = Instant::now();
        checksum ^= pool.frame()?;
        cpu_stages[index] = before.elapsed().as_nanos() as u64;
        let before = Instant::now();
        if let Some(gpu) = &mut gpu {
            gpu.submit((checksum ^ count) as u32)?;
        }
        submit_stages[index] = before.elapsed().as_nanos() as u64;
        let now = Instant::now();
        intervals[index] = now.duration_since(previous).as_nanos() as u64;
        previous = now;
        count += 1;
    }
    if let Some(gpu) = &mut gpu {
        for ns in gpu.drain()? {
            gpu_stages[gpu_samples] = ns;
            gpu_samples += 1;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let cpu_seconds = stats::cpu_time(false)
        .saturating_sub(cpu_start)
        .as_secs_f64();
    let scheduler = Scheduler::current().since(&schedule);
    ensure!(
        count > 0 && (!use_gpu || gpu_samples as u64 == count),
        "GPU completions disagree with frame count"
    );
    let worker_results = pool.finish()?;
    let last_hash = gpu.as_mut().map(gpu::Gpu::verify).transpose()?;
    ensure!(first_hash == last_hash, "GPU validation hash changed");
    let samples = count as usize;
    let mut report = json!({"completed_frames":count,"elapsed_seconds":elapsed,"frames_per_second":count as f64/elapsed,"cpu_process_seconds":cpu_seconds,"coordinator_scheduler":scheduler,"frame_intervals":stats::distribution(&mut intervals[..samples]),"cpu_dispatch_and_work":stats::distribution(&mut cpu_stages[..samples]),"gpu_record_and_submit":stats::distribution(&mut submit_stages[..samples]),"gpu_slot_wait":stats::distribution(&mut wait_stages[..samples]),"gpu_execution":stats::distribution(&mut gpu_stages[..gpu_samples]),"workers":worker_results,"worker_accounting_scope":"whole case including warmup; process CPU time excludes warmup and validation","checksum":checksum,"steady_state_readbacks":0,"diagnostic_readbacks":if use_gpu {2}else{0}});
    report["memory_pages"] = memory_pages;
    if let Some(gpu) = gpu {
        report["gpu"] = gpu.info.clone();
        report["gpu_known_answer_hash"] = json!(first_hash.context("missing verification hash")?);
        report["gpu_known_answer_verified"] = json!(true);
    }
    Ok(report)
}
