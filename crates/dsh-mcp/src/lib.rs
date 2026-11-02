#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! MCP stdio client and tool provider.
//!
//! [`McpPlugin`] starts each configured server, lists its tools, and registers
//! one [`McpTool`] per declaration. Tools share a single server process through
//! their [`McpConnection`], so a call never races another call on the same
//! pipe.

mod config;
mod connection;
mod error;
mod plugin;
mod protocol;
mod tool;

#[cfg(test)]
mod test_util;

pub use config::{McpServerConfig, SERVERS_ENV, parse_servers};
pub use connection::{McpConnection, ProcessConnection};
pub use error::McpError;
pub use plugin::McpPlugin;
pub use protocol::{McpToolResult, McpToolSpec};
pub use tool::McpTool;
