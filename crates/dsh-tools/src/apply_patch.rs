//! Apply a unified diff with `git apply`.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Apply a unified diff to the session workspace.
pub struct ApplyPatchTool {
    git: PathBuf,
}

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self::new("git")
    }
}

impl ApplyPatchTool {
    /// Build a tool using an explicit `git` executable.
    #[must_use]
    pub fn new(git: impl Into<PathBuf>) -> Self {
        Self { git: git.into() }
    }
}

#[derive(Deserialize)]
struct PatchArgs {
    patch: String,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "apply_patch".to_string(),
            description: "Apply a unified diff to files in the session workspace.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string"}
                },
                "required": ["patch"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: PatchArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args.patch.trim().is_empty() {
            return ToolOutcome::error("patch must not be empty");
        }
        let mut child = match Command::new(&self.git)
            .arg("-C")
            .arg(&context.cwd)
            .arg("apply")
            .arg("--whitespace=nowarn")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => return ToolOutcome::error(format!("cannot run git: {error}")),
        };
        let Some(mut stdin) = child.stdin.take() else {
            return ToolOutcome::error("cannot open git stdin");
        };
        if let Err(error) = stdin.write_all(args.patch.as_bytes()).await {
            return ToolOutcome::error(format!("cannot write patch: {error}"));
        }
        drop(stdin);
        let output = match child.wait_with_output().await {
            Ok(output) => output,
            Err(error) => return ToolOutcome::error(format!("git apply failed: {error}")),
        };
        if !output.status.success() {
            return ToolOutcome::error(format!(
                "git apply failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        ToolOutcome::text("applied patch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use std::os::unix::fs::PermissionsExt;

    fn fake_git(directory: &std::path::Path, success: bool) -> PathBuf {
        let script = directory.join("git.sh");
        let patch = directory.join("seen.patch");
        let args = directory.join("seen.args");
        let body = format!(
            "#!/bin/sh\ncat > {}\nprintf '%s\\n' \"$@\" > {}\n{}\n",
            patch.display(),
            args.display(),
            if success {
                "exit 0"
            } else {
                "echo boom >&2; exit 1"
            }
        );
        std::fs::write(&script, body).expect("script");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("permissions");
        script
    }

    #[tokio::test]
    async fn patches_are_forwarded_on_stdin() {
        let directory = tempfile::tempdir().expect("tempdir");
        let git = fake_git(directory.path(), true);
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ApplyPatchTool::new(git.clone())
            .execute(&context, json!({"patch": "--- a\n+++ b\n"}))
            .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        let seen = std::fs::read_to_string(directory.path().join("seen.patch")).expect("patch");
        assert_eq!(seen, "--- a\n+++ b\n");
        let args = std::fs::read_to_string(directory.path().join("seen.args")).expect("args");
        assert!(args.contains("apply"));
        assert!(args.contains("--whitespace=nowarn"));
    }

    #[tokio::test]
    async fn failing_git_and_empty_patches_are_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let git = fake_git(directory.path(), false);
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ApplyPatchTool::new(git)
            .execute(&context, json!({"patch": "--- a\n+++ b\n"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("git apply failed"));
        assert!(
            ApplyPatchTool::default()
                .execute(&context, json!({"patch": "  "}))
                .await
                .is_error
        );
    }
}
