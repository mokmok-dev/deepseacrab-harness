//! Failures raised by the LSP client.

use thiserror::Error;

/// Why a language server could not be configured, reached, or understood.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LspError {
    /// The server process could not be started or lost its pipes.
    #[error("lsp server {server}: {message}")]
    Server {
        /// Configured server name.
        server: String,
        /// Underlying failure.
        message: String,
    },
    /// The peer sent a frame that is not valid JSON-RPC.
    #[error("lsp protocol error: {0}")]
    Protocol(String),
    /// The peer answered with a JSON-RPC error object.
    #[error("lsp server returned an error: {0}")]
    Rpc(String),
    /// The server configuration is malformed.
    #[error("invalid lsp server configuration: {0}")]
    Config(String),
}
