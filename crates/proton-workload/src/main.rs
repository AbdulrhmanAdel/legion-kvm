#![cfg_attr(windows, no_std)]
#![cfg_attr(windows, no_main)]

#[cfg(windows)]
extern crate alloc;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
fn main() {
    eprintln!("build this executable for x86_64-pc-windows-msvc and run through Proton");
    std::process::exit(2);
}
