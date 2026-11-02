#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! LSP stdio client and tool provider.
//!
//! [`LspPlugin`] starts each configured language server and registers one
//! [`LspTool`] that routes an operation to the server whose extensions match
//! the file. Servers speak Content-Length framed JSON-RPC over stdio.

mod config;
mod connection;
mod error;
mod format;
mod framing;
mod plugin;
mod tool;

#[cfg(test)]
mod test_util;

pub use config::{LspServerConfig, SERVERS_ENV};
pub use connection::{LspConnection, ProcessConnection};
pub use error::LspError;
pub use plugin::LspPlugin;
pub use tool::{LspServer, LspTool};
