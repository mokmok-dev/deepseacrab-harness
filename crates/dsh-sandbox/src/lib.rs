//! Execution confinement providers.

use dsh_core::{NoSandbox, PermissionMode, Sandbox, SandboxCommand, SandboxMode};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Build the configured sandbox provider.
#[must_use]
pub fn sandbox(mode: SandboxMode) -> Arc<dyn Sandbox> {
    match mode {
        SandboxMode::Off => Arc::new(NoSandbox),
        SandboxMode::Bubblewrap => Arc::new(BubblewrapSandbox::new("bwrap")),
    }
}

/// Linux bubblewrap confinement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BubblewrapSandbox {
    /// Path to the `bwrap` executable.
    pub bwrap: PathBuf,
}

impl BubblewrapSandbox {
    /// Build a provider for one `bwrap` executable.
    #[must_use]
    pub fn new(bwrap: impl Into<PathBuf>) -> Self {
        Self {
            bwrap: bwrap.into(),
        }
    }
}

impl Sandbox for BubblewrapSandbox {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }

    fn wrap(&self, command: &str, cwd: &Path, permission: PermissionMode) -> SandboxCommand {
        if permission == PermissionMode::DangerFullAccess {
            return SandboxCommand::shell(command);
        }
        let cwd = cwd.to_string_lossy().into_owned();
        SandboxCommand {
            program: self.bwrap.to_string_lossy().into_owned(),
            args: vec![
                "--die-with-parent".to_string(),
                "--unshare-all".to_string(),
                "--share-net".to_string(),
                "--new-session".to_string(),
                "--ro-bind".to_string(),
                "/".to_string(),
                "/".to_string(),
                "--dev".to_string(),
                "/dev".to_string(),
                "--proc".to_string(),
                "/proc".to_string(),
                "--tmpfs".to_string(),
                "/tmp".to_string(),
                "--bind".to_string(),
                cwd.clone(),
                cwd.clone(),
                "--chdir".to_string(),
                cwd,
                "--".to_string(),
                "bash".to_string(),
                "-lc".to_string(),
                command.to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_returns_plain_shell() {
        let provider = sandbox(SandboxMode::Off);
        assert_eq!(provider.name(), "none");
        assert_eq!(
            provider.wrap(
                "echo hi",
                Path::new("/work"),
                PermissionMode::WorkspaceWrite
            ),
            SandboxCommand::shell("echo hi")
        );
    }

    #[test]
    fn bubblewrap_binds_the_workspace() {
        let provider = sandbox(SandboxMode::Bubblewrap);
        assert_eq!(provider.name(), "bubblewrap");
        let command = provider.wrap(
            "echo hi",
            Path::new("/work"),
            PermissionMode::WorkspaceWrite,
        );
        assert_eq!(command.program, "bwrap");
        assert!(command.args.iter().any(|arg| arg == "--bind"));
        assert!(command.args.iter().any(|arg| arg == "/work"));
        assert!(command.args.iter().any(|arg| arg == "--chdir"));
        assert_eq!(command.args.last().map(String::as_str), Some("echo hi"));
    }

    #[test]
    fn full_access_skips_confinement() {
        let provider = BubblewrapSandbox::new("bwrap");
        assert_eq!(
            provider.wrap(
                "echo hi",
                Path::new("/work"),
                PermissionMode::DangerFullAccess
            ),
            SandboxCommand::shell("echo hi")
        );
    }
}
