#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Remote execution over SSH.
//!
//! [`SshPlugin`] loads host definitions from `DSH_SSH_HOSTS` and registers one
//! [`SshTool`] that runs a command on a named host. The command reaches `ssh`
//! as a single argument after `--`, so no local shell parses it.

mod config;
mod error;
mod plugin;
mod tool;

pub use config::{DEFAULT_SSH, HOSTS_ENV, SshHost};
pub use error::SshError;
pub use plugin::SshPlugin;
pub use tool::SshTool;
