//! Execution confinement seam for shell and terminal tools.

use crate::tool::PermissionMode;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One resolved command line ready for a process builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    /// Program to execute.
    pub program: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
}

impl SandboxCommand {
    /// Plain `bash -lc` execution.
    #[must_use]
    pub fn shell(command: &str) -> Self {
        Self {
            program: "bash".to_string(),
            args: vec!["-lc".to_string(), command.to_string()],
        }
    }
}

/// A confinement provider for shell-like execution.
pub trait Sandbox: Send + Sync {
    /// Stable provider name used in diagnostics.
    fn name(&self) -> &'static str;

    /// Resolve a command into the program and arguments to spawn.
    fn wrap(&self, command: &str, cwd: &Path, permission: PermissionMode) -> SandboxCommand;
}

/// No confinement beyond the permission policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    fn wrap(&self, command: &str, _cwd: &Path, _permission: PermissionMode) -> SandboxCommand {
        SandboxCommand::shell(command)
    }
}

/// Deployment choice for the execution sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Permission policy only.
    #[default]
    Off,
    /// Linux bubblewrap confinement.
    Bubblewrap,
}

impl SandboxMode {
    /// Parse the spelling accepted by configuration.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "bubblewrap" => Some(Self::Bubblewrap),
            _ => None,
        }
    }

    /// Stable configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Bubblewrap => "bubblewrap",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn plain_shell_keeps_the_command_line() {
        assert_eq!(
            SandboxCommand::shell("echo hi"),
            SandboxCommand {
                program: "bash".to_string(),
                args: vec!["-lc".to_string(), "echo hi".to_string()],
            }
        );
    }

    #[test]
    fn no_sandbox_uses_plain_shell() {
        let command = NoSandbox.wrap(
            "echo hi",
            &PathBuf::from("/work"),
            PermissionMode::WorkspaceWrite,
        );
        assert_eq!(command, SandboxCommand::shell("echo hi"));
        assert_eq!(NoSandbox.name(), "none");
    }

    #[test]
    fn sandbox_modes_round_trip() {
        for raw in ["off", "bubblewrap"] {
            assert_eq!(SandboxMode::parse(raw).expect("known mode").as_str(), raw);
        }
        assert_eq!(SandboxMode::parse("docker"), None);
        assert_eq!(SandboxMode::default(), SandboxMode::Off);
    }
}
