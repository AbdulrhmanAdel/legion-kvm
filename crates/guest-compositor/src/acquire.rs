//! Bounded, ordered acquisition.
use std::collections::VecDeque;

use anyhow::{Result, ensure};

pub(crate) struct Acquired<T> {
    pub resource: u64,
    pub frame: u64,
    pub payload: T,
}

pub(crate) struct Queue<T> {
    entries: VecDeque<Acquired<T>>,
    capacity: usize,
}

impl<T> Queue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn contains(&self, resource: u64) -> bool {
        self.entries.iter().any(|entry| entry.resource == resource)
    }

    pub fn push(&mut self, resource: u64, frame: u64, payload: T) -> Result<()> {
        ensure!(!self.is_full(), "acquire queue full");
        ensure!(
            frame > 0 && !self.contains(resource),
            "duplicate acquisition or invalid frame"
        );
        self.entries.push_back(Acquired {
            resource,
            frame,
            payload,
        });
        Ok(())
    }

    pub fn pop_ready(
        &mut self,
        ready: impl FnOnce(&T) -> Result<bool>,
    ) -> Result<Option<Acquired<T>>> {
        let Some(front) = self.entries.front() else {
            return Ok(None);
        };
        if ready(&front.payload)? {
            Ok(self.entries.pop_front())
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/acquire.rs"]
mod tests;
