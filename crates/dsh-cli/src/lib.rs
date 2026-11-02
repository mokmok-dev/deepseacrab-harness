#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Headless command-line surface for the harness.

pub mod cli;
pub mod commands;
mod providers;

pub use commands::run;
