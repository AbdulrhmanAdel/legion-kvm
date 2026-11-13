//! Ordered input for the owned presentation window.
use anyhow::{Result, ensure};
use std::collections::VecDeque;

pub const BYTES: usize = 1024;
pub const MAX_EVENTS: usize = 20;
pub const MAX_KEYMAP: usize = 1024 * 1024;
pub const KEYMAP_CHUNK: usize = BYTES - 32;
pub const QUEUE_CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub enum CaptureMode {
    #[default]
    None = 0,
    Lock = 1,
    Confine = 2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Key {
        code: u32,
        down: bool,
    },
    Button {
        code: u32,
        down: bool,
    },
    Absolute {
        x: f64,
        y: f64,
    },
    Relative {
        dx: f64,
        dy: f64,
        raw_dx: f64,
        raw_dy: f64,
    },
    Axis {
        axis: u32,
        value: f64,
        v120: i32,
        stop: bool,
        source: u32,
    },
    Focus {
        keyboard: bool,
        focused: bool,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    Reset,
    Capture {
        active: bool,
    },
    Repeat {
        rate: u32,
        delay: u32,
    },
    KeyboardRestore,
    HeldKey {
        code: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedEvent {
    pub time_us: u64,
    pub event: Event,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Batch {
    pub width: u32,
    pub height: u32,
    pub closed: bool,
    pub relative_supported: bool,
    pub capture_active: bool,
    pub keymap_generation: u32,
    pub sequence: u64,
    pub overflows: u64,
    pub pending: u32,
    pub events: Vec<TimedEvent>,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("bounded input word"))
}
fn wide(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("bounded input word"))
}
fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

impl Event {
    fn fields(self) -> (u32, u32, [u64; 4]) {
        match self {
            Self::Key { code, down } => (1, code, [u64::from(down), 0, 0, 0]),
            Self::Button { code, down } => (2, code, [u64::from(down), 0, 0, 0]),
            Self::Absolute { x, y } => (3, 0, [x.to_bits(), y.to_bits(), 0, 0]),
            Self::Relative {
                dx,
                dy,
                raw_dx,
                raw_dy,
            } => (
                4,
                0,
                [
                    dx.to_bits(),
                    dy.to_bits(),
                    raw_dx.to_bits(),
                    raw_dy.to_bits(),
                ],
            ),
            Self::Axis {
                axis,
                value,
                v120,
                stop,
                source,
            } => (
                5,
                axis,
                [
                    value.to_bits(),
                    v120 as i64 as u64,
                    u64::from(stop),
                    u64::from(source),
                ],
            ),
            Self::Focus { keyboard, focused } => {
                (6, u32::from(keyboard), [u64::from(focused), 0, 0, 0])
            }
            Self::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => (
                7,
                0,
                [
                    depressed.into(),
                    latched.into(),
                    locked.into(),
                    group.into(),
                ],
            ),
            Self::Reset => (8, 0, [0; 4]),
            Self::Capture { active } => (9, 0, [u64::from(active), 0, 0, 0]),
            Self::Repeat { rate, delay } => (10, 0, [rate.into(), delay.into(), 0, 0]),
            Self::KeyboardRestore => (11, 0, [0; 4]),
            Self::HeldKey { code } => (12, code, [0; 4]),
        }
    }
    fn parse(kind: u32, code: u32, values: [u64; 4]) -> Result<Self> {
        let [a, b, c, d] = values;
        let zero_tail = b == 0 && c == 0 && d == 0;
        let finite = |bits, limit: f64| {
            let value = f64::from_bits(bits);
            value.is_finite() && value.abs() <= limit
        };
        Ok(match kind {
            1 if code < 768 && a <= 1 && zero_tail => Self::Key { code, down: a != 0 },
            2 if (0x110..=0x11f).contains(&code) && a <= 1 && zero_tail => {
                Self::Button { code, down: a != 0 }
            }
            3 if code == 0 && finite(a, 16.0) && finite(b, 16.0) && c == 0 && d == 0 => {
                Self::Absolute {
                    x: f64::from_bits(a),
                    y: f64::from_bits(b),
                }
            }
            4 if code == 0 && values.iter().all(|v| finite(*v, 1e6)) => Self::Relative {
                dx: f64::from_bits(a),
                dy: f64::from_bits(b),
                raw_dx: f64::from_bits(c),
                raw_dy: f64::from_bits(d),
            },
            5 if code <= 1
                && finite(a, 1e6)
                && i32::try_from(b as i64).is_ok()
                && c <= 1
                && d <= 3 =>
            {
                Self::Axis {
                    axis: code,
                    value: f64::from_bits(a),
                    v120: b as i64 as i32,
                    stop: c != 0,
                    source: d as u32,
                }
            }
            6 if code <= 1 && a <= 1 && zero_tail => Self::Focus {
                keyboard: code != 0,
                focused: a != 0,
            },
            7 if code == 0 && values.iter().all(|v| u32::try_from(*v).is_ok()) => Self::Modifiers {
                depressed: a as u32,
                latched: b as u32,
                locked: c as u32,
                group: d as u32,
            },
            8 if code == 0 && values == [0; 4] => Self::Reset,
            9 if code == 0 && a <= 1 && zero_tail => Self::Capture { active: a != 0 },
            10 if code == 0 && a <= 1000 && b <= 60_000 && c == 0 && d == 0 => Self::Repeat {
                rate: a as u32,
                delay: b as u32,
            },
            11 if code == 0 && values == [0; 4] => Self::KeyboardRestore,
            12 if code < 768 && values == [0; 4] => Self::HeldKey { code },
            _ => anyhow::bail!("invalid or unsupported input event"),
        })
    }
}

impl Batch {
    pub fn request(capture: CaptureMode, bytes: &mut [u8]) -> Result<()> {
        ensure!(bytes.len() == BYTES, "invalid input request size");
        bytes.fill(0);
        put(bytes, 0, 1);
        put(bytes, 8, capture as u32);
        Ok(())
    }
    pub fn capture_request(bytes: &[u8]) -> Result<CaptureMode> {
        ensure!(
            bytes.len() == BYTES
                && word(bytes, 0) == 1
                && word(bytes, 8) <= 2
                && bytes[4..8].iter().all(|v| *v == 0)
                && bytes[12..].iter().all(|v| *v == 0),
            "invalid input request"
        );
        Ok(match word(bytes, 8) {
            1 => CaptureMode::Lock,
            2 => CaptureMode::Confine,
            _ => CaptureMode::None,
        })
    }
    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        ensure!(
            bytes.len() == BYTES && self.events.len() <= MAX_EVENTS,
            "invalid input batch size"
        );
        ensure!(
            (1..=3840).contains(&self.width)
                && (1..=2160).contains(&self.height)
                && self.pending as usize <= QUEUE_CAPACITY,
            "invalid input batch geometry or backlog"
        );
        bytes.fill(0);
        put(bytes, 0, 1);
        put(bytes, 4, self.events.len() as u32);
        put(
            bytes,
            8,
            u32::from(self.closed)
                | u32::from(self.relative_supported) << 1
                | u32::from(self.capture_active) << 2,
        );
        put(bytes, 12, self.width);
        put(bytes, 16, self.height);
        put(bytes, 20, self.keymap_generation);
        bytes[24..32].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.overflows.to_le_bytes());
        put(bytes, 40, self.pending);
        for (index, event) in self.events.iter().enumerate() {
            let at = 64 + index * 48;
            let (kind, code, values) = event.event.fields();
            Event::parse(kind, code, values)?;
            put(bytes, at, kind);
            put(bytes, at + 4, code);
            bytes[at + 8..at + 16].copy_from_slice(&event.time_us.to_le_bytes());
            for (i, value) in values.iter().enumerate() {
                bytes[at + 16 + i * 8..at + 24 + i * 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        Ok(())
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == BYTES && word(bytes, 0) == 1,
            "invalid input batch version/size"
        );
        let count = word(bytes, 4) as usize;
        ensure!(
            count <= MAX_EVENTS
                && word(bytes, 8) & !7 == 0
                && bytes[44..64].iter().all(|v| *v == 0)
                && bytes[64 + count * 48..].iter().all(|v| *v == 0),
            "invalid input batch flags/count/padding"
        );
        let mut batch = Self {
            width: word(bytes, 12),
            height: word(bytes, 16),
            closed: word(bytes, 8) & 1 != 0,
            relative_supported: word(bytes, 8) & 2 != 0,
            capture_active: word(bytes, 8) & 4 != 0,
            keymap_generation: word(bytes, 20),
            sequence: wide(bytes, 24),
            overflows: wide(bytes, 32),
            pending: word(bytes, 40),
            events: Vec::with_capacity(count),
        };
        ensure!(
            (1..=3840).contains(&batch.width)
                && (1..=2160).contains(&batch.height)
                && batch.pending as usize <= QUEUE_CAPACITY,
            "invalid input batch geometry/backlog"
        );
        for index in 0..count {
            let at = 64 + index * 48;
            batch.events.push(TimedEvent {
                time_us: wide(bytes, at + 8),
                event: Event::parse(
                    word(bytes, at),
                    word(bytes, at + 4),
                    core::array::from_fn(|i| wide(bytes, at + 16 + i * 8)),
                )?,
            });
        }
        Ok(batch)
    }
}

/// Input is queued, including both edges of presses shorter than a poll.
#[derive(Debug, Default)]
pub struct Queue {
    events: VecDeque<TimedEvent>,
    served: u64,
    overflows: u64,
}
impl Queue {
    pub fn reset(&mut self, time_us: u64) {
        self.events.clear();
        self.events.push_back(TimedEvent {
            time_us,
            event: Event::Reset,
        });
    }
    pub fn push(&mut self, event: TimedEvent) {
        if let Some(previous) = self.events.back_mut()
            && let (Event::Absolute { x, y }, Event::Absolute { x: a, y: b }) =
                (&mut previous.event, event.event)
        {
            *x = a;
            *y = b;
            previous.time_us = event.time_us;
            return;
        }
        if self.events.len() == QUEUE_CAPACITY {
            self.events.clear();
            self.overflows = self.overflows.saturating_add(1);
            self.events.push_back(TimedEvent {
                time_us: event.time_us,
                event: Event::Reset,
            });
        }
        self.events.push_back(event);
    }
    pub fn drain(&mut self, batch: &mut Batch) {
        batch.events = self
            .events
            .drain(..self.events.len().min(MAX_EVENTS))
            .collect();
        self.served = self.served.saturating_add(batch.events.len() as u64);
        batch.sequence = self.served;
        batch.overflows = self.overflows;
        batch.pending = self.events.len() as u32;
    }
}

pub fn keymap_request(generation: u32, offset: u32, bytes: &mut [u8]) -> Result<()> {
    ensure!(
        bytes.len() == BYTES && offset as usize <= MAX_KEYMAP,
        "invalid keymap request"
    );
    bytes.fill(0);
    put(bytes, 0, 1);
    put(bytes, 4, generation);
    put(bytes, 8, offset);
    Ok(())
}
pub fn keymap_reply(keymap: &str, generation: u32, bytes: &mut [u8]) -> Result<()> {
    ensure!(
        bytes.len() == BYTES
            && word(bytes, 0) == 1
            && bytes[12..].iter().all(|b| *b == 0)
            && word(bytes, 4) == generation
            && keymap.len() <= MAX_KEYMAP,
        "keymap generation changed or invalid request"
    );
    let offset = word(bytes, 8) as usize;
    ensure!(offset <= keymap.len(), "keymap offset out of bounds");
    let size = (keymap.len() - offset).min(KEYMAP_CHUNK);
    put(bytes, 12, keymap.len() as u32);
    put(bytes, 16, size as u32);
    bytes[32..32 + size].copy_from_slice(&keymap.as_bytes()[offset..offset + size]);
    Ok(())
}
pub fn keymap_chunk(bytes: &[u8], generation: u32, offset: usize) -> Result<(usize, &[u8])> {
    ensure!(
        bytes.len() == BYTES
            && word(bytes, 0) == 1
            && word(bytes, 4) == generation
            && word(bytes, 8) as usize == offset,
        "invalid keymap reply identity"
    );
    let total = word(bytes, 12) as usize;
    let size = word(bytes, 16) as usize;
    ensure!(
        total <= MAX_KEYMAP
            && size <= KEYMAP_CHUNK
            && offset.checked_add(size).is_some_and(|end| end <= total)
            && bytes[20..32].iter().all(|b| *b == 0)
            && bytes[32 + size..].iter().all(|b| *b == 0),
        "invalid keymap reply extent/padding"
    );
    Ok((total, &bytes[32..32 + size]))
}

#[cfg(test)]
#[path = "../tests/unit/input_events.rs"]
mod tests;
