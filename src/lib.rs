//! `gitaur` library crate. Same module tree as the binary; `main.rs` thin-wraps
//! [`cli::run`]. Exposed here so `tests/` integration suites can drive
//! individual layers (mirror, index, resolver) directly.

pub mod build;
pub mod cli;
pub mod config;
pub mod error;
pub mod index;
pub mod mirror;
pub mod pacman;
pub mod paths;
pub mod resolver;
pub mod ui;

#[doc(hidden)]
pub mod testing;
