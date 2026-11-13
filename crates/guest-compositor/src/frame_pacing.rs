use std::{
    fs::File,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::frame_transport::call;

pub(crate) struct Pacer {
    enabled: bool,
    credit: bool,
    callbacks: u64,
    queries: u64,
    last_query: Instant,
}

impl Pacer {
    pub fn new(mode: &str) -> Result<Self> {
        ensure!(
            matches!(mode, "callbacks" | "unpaced"),
            "unknown frame pacing"
        );
        Ok(Self {
            enabled: mode == "callbacks",
            credit: true,
            callbacks: 0,
            queries: 0,
            last_query: Instant::now(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The native control observes real desktop callbacks without the VM RPC.
    pub fn callbacks_ready(&mut self, callbacks: u64) -> Result<bool> {
        self.observe(callbacks)?;
        Ok(!self.enabled || self.credit)
    }

    pub fn ready(&mut self, fd: &File) -> Result<bool> {
        if !self.enabled || self.credit {
            return Ok(true);
        }
        if self.last_query.elapsed() >= Duration::from_millis(1) {
            let mut packet = [0; 40];
            call(fd, 6, &mut packet)?;
            let callbacks =
                u64::from_le_bytes(packet[24..32].try_into().expect("fixed statistics"));
            self.observe(callbacks)?;
            self.queries += 1;
            self.last_query = Instant::now();
        }
        Ok(self.credit)
    }

    pub fn consume(&mut self) -> Result<()> {
        ensure!(!self.enabled || self.credit, "output frame has no credit");
        self.credit = false;
        Ok(())
    }

    /// A source retired before submission cannot generate an output callback.
    pub fn cancel_unsubmitted(&mut self) {
        self.credit = true;
    }

    fn observe(&mut self, callbacks: u64) -> Result<()> {
        ensure!(
            callbacks >= self.callbacks,
            "output callback counter regressed"
        );
        if callbacks > self.callbacks {
            self.credit = true;
            self.callbacks = callbacks;
        }
        Ok(())
    }

    pub fn report(&self) -> Value {
        json!({"mode":if self.enabled {"callbacks"} else {"unpaced"}, "output_callbacks":self.callbacks, "status_queries":self.queries})
    }
}

#[cfg(test)]
#[path = "../tests/unit/frame_pacing.rs"]
mod tests;
