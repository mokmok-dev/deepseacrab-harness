//! Failures raised by the MCP client.

use thiserror::Error;

/// Why an MCP server could not be configured, reached, or understood.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum McpError {
    /// The server process could not be started or lost its pipes.
    #[error("mcp server {server}: {message}")]
    Server {
        /// Configured server name.
        server: String,
        /// Underlying failure.
        message: String,
    },
    /// The peer sent a frame that is not valid JSON-RPC for this client.
    #[error("mcp protocol error: {0}")]
    Protocol(String),
    /// The peer answered with a JSON-RPC error object.
    #[error("mcp server returned an error: {0}")]
    Rpc(String),
    /// The server configuration is malformed.
    #[error("invalid mcp server configuration: {0}")]
    Config(String),
}
