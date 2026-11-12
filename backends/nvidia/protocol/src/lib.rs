// crates/protocol/src/lib.rs Wire protocol shared between the VMM backend (Rust) and the guest kernel driver (C).

#![no_std]

pub mod messages;
pub mod pageruns;
pub mod segments;

pub use messages::*;
