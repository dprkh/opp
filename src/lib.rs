#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

#[cfg(not(target_os = "macos"))]
compile_error!("opp supports macOS only");

mod account;
mod broker;
mod cli;
mod client;
mod clock;
mod darwin;
mod environment;
mod process;
mod protocol;
mod runtime;
mod status;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() -> i32 {
    cli::run(std::env::args_os())
}
