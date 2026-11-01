//! Shared fixtures for tool tests.

use dsh_core::{PermissionMode, ToolContext};
use std::path::Path;
use tokio_util::sync::CancellationToken;

#[must_use]
pub fn context(directory: &Path, permission: PermissionMode) -> ToolContext {
    ToolContext::new(
        directory.to_path_buf(),
        permission,
        CancellationToken::new(),
    )
}
