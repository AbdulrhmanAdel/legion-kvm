//! Repeatable ALU, scene-update and dependent-load kernels.
use crate::{Case, Kernel};
use anyhow::{Result, ensure};
use std::hint::black_box;

pub enum State {
    Integer(Vec<u64>),
    Scene(Vec<[f32; 8]>),
    Memory { next: Vec<u32>, cursor: u32 },
}

fn random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

impl State {
    pub fn new(case: &Case, worker: usize) -> Result<Self> {
        ensure!(
            case.working_set_kib > 0 && case.working_set_kib <= 16384,
            "invalid working set"
        );
        let bytes = case.working_set_kib * 1024;
        let mut seed = 0x4c45_4749_4f4e_0001_u64.wrapping_add(worker as u64);
        Ok(match case.kernel {
            Kernel::Integer => Self::Integer((0..bytes / 8).map(|_| random(&mut seed)).collect()),
            Kernel::Scene => Self::Scene(
                (0..bytes / 32)
                    .map(|_| {
                        std::array::from_fn(|_| (random(&mut seed) & 65535) as f32 / 65536.0 - 0.5)
                    })
                    .collect(),
            ),
            Kernel::Memory => {
                let n = bytes / 4;
                let mut order: Vec<u32> = (0..n as u32).collect();
                for i in (1..n).rev() {
                    order.swap(i, (random(&mut seed) % (i + 1) as u64) as usize);
                }
                let mut next = vec![0; n];
                for i in 0..n {
                    next[order[i] as usize] = order[(i + 1) % n];
                }
                Self::Memory { next, cursor: 0 }
            }
        })
    }

    #[inline(never)]
    pub fn job(&mut self, units: u32) -> u64 {
        black_box(match self {
            Self::Integer(data) => {
                for _ in 0..units {
                    for value in data.iter_mut() {
                        *value = value.wrapping_mul(0xd6e8_feb8_6659_fd93).rotate_left(23)
                            ^ (*value >> 17);
                    }
                }
                data[0]
            }
            Self::Scene(data) => {
                for _ in 0..units {
                    for entity in data.iter_mut() {
                        let [x, y, z, _, vx, vy, vz, _] = *entity;
                        entity[0] = x * 0.999 + vx * 0.03125;
                        entity[1] = y * 0.999 + vy * 0.03125;
                        entity[2] = z * 0.999 + vz * 0.03125;
                        entity[3] = x * 0.36 + y * -0.48 + z * 0.8;
                        // Small forcing keeps long runs away from denormals and an all-zero fixed point as damping progresses.
                        entity[4] = vx * 0.998 - x * 0.0002 + 0.00001;
                        entity[5] = vy * 0.998 - y * 0.0002 - 0.00002;
                        entity[6] = vz * 0.998 - z * 0.0002 + 0.00003;
                        entity[7] = x * 0.8 + y * 0.6;
                    }
                }
                data[0][0].to_bits() as u64
            }
            Self::Memory { next, cursor } => {
                for _ in 0..units * 8192 {
                    *cursor = next[*cursor as usize];
                }
                u64::from(*cursor)
            }
        })
    }

    pub fn checksum(&self) -> u64 {
        let fold = |acc: u64, n: u64| {
            acc.rotate_left(7)
                .wrapping_add(n)
                .wrapping_mul(0x100000001b3)
        };
        match self {
            Self::Integer(data) => data.iter().fold(0, |a, &n| fold(a, n)),
            Self::Scene(data) => data
                .iter()
                .flatten()
                .fold(0, |a, n| fold(a, u64::from(n.to_bits()))),
            Self::Memory { next, cursor } => next
                .iter()
                .fold(u64::from(*cursor), |a, &n| fold(a, u64::from(n))),
        }
    }
}

pub fn known_answer(case: &Case, worker: usize) -> Result<u64> {
    let mut state = State::new(case, worker)?;
    for _ in 0..4 {
        state.job(case.work_units);
    }
    Ok(state.checksum())
}

#[cfg(test)]
#[path = "../tests/unit/cpu.rs"]
mod tests;
