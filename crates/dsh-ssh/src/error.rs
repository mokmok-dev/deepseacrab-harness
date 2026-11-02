//! Failures raised while loading SSH host configuration.

use thiserror::Error;

/// Why SSH hosts could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SshError {
    /// The host document is malformed or violates a host invariant.
    #[error("invalid ssh host configuration: {0}")]
    Config(String),
}
