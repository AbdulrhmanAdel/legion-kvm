//! Persistent workers for frame-style dispatch and independent throughput.
use crate::{
    Case, cpu,
    stats::{self, Scheduler},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Barrier, OnceLock, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Worker {
    commands: mpsc::SyncSender<bool>,
    join: Option<JoinHandle<Value>>,
}

pub struct Pool {
    workers: Vec<Worker>,
    completed: mpsc::Receiver<u64>,
}

impl Pool {
    pub fn new(case: &Case) -> Result<Self> {
        let (done, completed) = mpsc::channel();
        let mut pool = Self {
            workers: Vec::with_capacity(case.workers),
            completed,
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        for id in 0..case.workers {
            let mut state = cpu::State::new(case, id)?;
            let answer = cpu::known_answer(case, id)?;
            let (commands, receiver) = mpsc::sync_channel::<bool>(1);
            let units = case.work_units;
            let (done, ready) = (done.clone(), ready_tx.clone());
            let join = thread::Builder::new().name(format!("workload-{id}")).spawn(move || {
                let mut jobs = 0_u64;
                let mut before = Scheduler::current();
                let mut cpu_start = stats::cpu_time(true);
                let _ = ready.send(());
                while let Ok(run) = receiver.recv() {
                    if !run { break; }
                    if jobs == 0 { before = Scheduler::current(); cpu_start = stats::cpu_time(true); }
                    let token = state.job(units); jobs += 1;
                    if done.send(token).is_err() { break; }
                }
                let cpu_s = stats::cpu_time(true).saturating_sub(cpu_start).as_secs_f64();
                json!({"worker":id,"jobs":jobs,"cpu_seconds":cpu_s,"scheduler":Scheduler::current().since(&before),"known_answer":answer,"final_checksum":state.checksum()})
            }).context("spawn workload worker")?;
            pool.workers.push(Worker {
                commands,
                join: Some(join),
            });
        }
        for _ in 0..case.workers {
            ready_rx.recv().context("worker failed initialization")?;
        }
        Ok(pool)
    }

    pub fn frame(&self) -> Result<u64> {
        for worker in &self.workers {
            worker
                .commands
                .send(true)
                .context("workload worker exited")?;
        }
        let mut token = 0;
        for _ in &self.workers {
            token ^= self
                .completed
                .recv()
                .context("workload result channel closed")?;
        }
        Ok(token)
    }

    pub fn finish(&mut self) -> Result<Vec<Value>> {
        for worker in &self.workers {
            let _ = worker.commands.send(false);
        }
        self.workers
            .iter_mut()
            .filter_map(|w| w.join.take())
            .map(|j| {
                j.join()
                    .map_err(|_| anyhow::anyhow!("workload worker panicked"))
            })
            .collect()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub fn throughput(case: &Case, warmup: Duration, sample: Duration) -> Result<Value> {
    let barrier = Arc::new(Barrier::new(case.workers + 1));
    let start = Arc::new(OnceLock::<Instant>::new());
    let mut states = Vec::new();
    for id in 0..case.workers {
        states.push((cpu::State::new(case, id)?, cpu::known_answer(case, id)?));
    }

    let (workers, memory_pages) = thread::scope(|scope| -> Result<(Vec<Value>, Value)> {
        let mut joins = Vec::new();
        for (id, (mut state, answer)) in states.into_iter().enumerate() {
            let (barrier, start) = (barrier.clone(), start.clone());
            joins.push(scope.spawn(move || {
                let end = Instant::now()+warmup;
                while Instant::now()<end { state.job(case.work_units); }
                barrier.wait(); barrier.wait();
                let begin = *start.get().expect("coordinator publishes start before release");
                let sched = Scheduler::current();
                let cpu = stats::cpu_time(true);
                let mut jobs = 0_u64;
                while begin.elapsed()<sample { state.job(case.work_units); jobs += 1; }
                let end = begin.elapsed().as_secs_f64();
                let cpu_s = stats::cpu_time(true).saturating_sub(cpu).as_secs_f64();
                json!({"worker":id,"jobs":jobs,"end_seconds":end,"cpu_seconds":cpu_s,"scheduler":Scheduler::current().since(&sched),"known_answer":answer,"final_checksum":state.checksum()})
            }));
        }
        barrier.wait();
        let memory_pages = stats::memory_pages();
        start
            .set(Instant::now())
            .map_err(|_| anyhow::anyhow!("duplicate measurement start"))?;
        barrier.wait();
        let workers = joins
            .into_iter()
            .map(|j| j.join().map_err(|_| anyhow::anyhow!("CPU worker panicked")))
            .collect::<Result<Vec<_>>>()?;
        Ok((workers, memory_pages))
    })?;
    let wall = workers
        .iter()
        .filter_map(|w| w["end_seconds"].as_f64())
        .fold(0.0, f64::max);
    let jobs: u64 = workers.iter().filter_map(|w| w["jobs"].as_u64()).sum();
    let cpu_seconds: f64 = workers
        .iter()
        .filter_map(|w| w["cpu_seconds"].as_f64())
        .sum();
    ensure!(
        wall > 0.0 && jobs > 0 && cpu_seconds > 0.0,
        "CPU workload completed no work"
    );
    Ok(
        json!({"completed_jobs":jobs,"elapsed_seconds":wall,"jobs_per_second":jobs as f64/wall,"worker_cpu_seconds":cpu_seconds,"jobs_per_cpu_second":jobs as f64/cpu_seconds,"cpu_busy_equivalent":cpu_seconds/wall,"workers":workers,"memory_pages":memory_pages}),
    )
}

#[cfg(all(test, not(miri)))]
#[path = "../tests/unit/jobs.rs"]
mod tests;
