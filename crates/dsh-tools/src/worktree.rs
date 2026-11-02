//! Create a Git worktree for an isolated task.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{SessionEvent, Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::process::Command;

/// Create a branch and worktree under the session directory.
pub struct CreateWorktreeTool {
    git: PathBuf,
}

impl Default for CreateWorktreeTool {
    fn default() -> Self {
        Self::new("git")
    }
}

impl CreateWorktreeTool {
    /// Build a tool using an explicit `git` executable.
    #[must_use]
    pub fn new(git: impl Into<PathBuf>) -> Self {
        Self { git: git.into() }
    }
}

#[derive(Deserialize)]
struct WorktreeArgs {
    branch: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    base: Option<String>,
}

#[async_trait]
impl Tool for CreateWorktreeTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "create_worktree".to_string(),
            description:
                "Create a new branch and Git worktree under the session directory, then switch the session directory to it."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "branch": {"type": "string"},
                    "path": {"type": "string"},
                    "base": {"type": "string"}
                },
                "required": ["branch"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: WorktreeArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if !valid_ref(&args.branch) {
            return ToolOutcome::error(
                "branch must be a simple Git ref without spaces or path segments",
            );
        }
        let base = args.base.unwrap_or_else(|| "HEAD".to_string());
        if !valid_ref(&base) {
            return ToolOutcome::error("base must be a simple Git ref");
        }
        let raw_path = args
            .path
            .unwrap_or_else(|| format!(".worktrees/{}", args.branch.replace('/', "-")));
        let path = match context.writable_path(&raw_path) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        if path.exists() {
            return ToolOutcome::error(format!("{} already exists", path.display()));
        }
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(&context.cwd)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&args.branch)
            .arg(&path)
            .arg(&base)
            .output()
            .await;
        let output = match output {
            Ok(output) => output,
            Err(error) => return ToolOutcome::error(format!("cannot run git: {error}")),
        };
        if !output.status.success() {
            return ToolOutcome::error(format!(
                "git worktree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if let Some(session) = context.session() {
            if let Err(error) = session.append(SessionEvent::WorkingDirectoryChanged {
                cwd: path.display().to_string(),
            }) {
                return ToolOutcome::error(error.to_string());
            }
        }
        ToolOutcome::text(format!(
            "created worktree {} on {}",
            path.display(),
            args.branch
        ))
    }
}

fn valid_ref(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.chars().any(|character| {
            character.is_whitespace()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::session_context;
    use dsh_core::PermissionMode;
    use std::os::unix::fs::PermissionsExt;

    fn fake_git(log: &std::path::Path) -> PathBuf {
        let script = log.with_extension("sh");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 0\n",
            log.display()
        );
        std::fs::write(&script, body).expect("script");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("permissions");
        script
    }

    #[tokio::test]
    async fn worktrees_are_created_and_the_session_moves() {
        let directory = tempfile::tempdir().expect("tempdir");
        let log = directory.path().join("git.log");
        let tool = CreateWorktreeTool::new(fake_git(&log));
        let (context, session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = tool
            .execute(
                &context,
                json!({"branch": "feature/demo", "path": "worktrees/demo", "base": "HEAD"}),
            )
            .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        let recorded = std::fs::read_to_string(&log).expect("log");
        for expected in [
            "worktree",
            "add",
            "-b",
            "feature/demo",
            "worktrees/demo",
            "HEAD",
        ] {
            assert!(
                recorded.contains(expected),
                "missing {expected} in {recorded}"
            );
        }
        assert!(matches!(
            &session.events()[0],
            SessionEvent::WorkingDirectoryChanged { cwd }
                if cwd.ends_with("worktrees/demo")
        ));
    }

    #[tokio::test]
    async fn failing_git_is_a_tool_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let script = directory.path().join("git.sh");
        std::fs::write(&script, "#!/bin/sh\necho boom >&2\nexit 1\n").expect("script");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("permissions");
        let (context, _session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = CreateWorktreeTool::new(script)
            .execute(&context, json!({"branch": "feature"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("git worktree failed"));
    }

    #[tokio::test]
    async fn invalid_refs_and_paths_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (context, _session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let tool = CreateWorktreeTool::new("git");
        for branch in ["", "has space", "../escape", "-danger", "a:b", "a..b"] {
            assert!(
                tool.execute(&context, json!({"branch": branch}))
                    .await
                    .is_error
            );
        }
        assert!(
            tool.execute(&context, json!({"branch": "ok", "path": "../outside"}))
                .await
                .is_error
        );
        let read_only = crate::test_util::context(directory.path(), PermissionMode::ReadOnly);
        assert!(
            tool.execute(&read_only, json!({"branch": "ok"}))
                .await
                .is_error
        );
    }

    #[test]
    fn ref_validation_accepts_common_branches() {
        for branch in ["main", "feature/demo", "fix-1", "a.b_c"] {
            assert!(valid_ref(branch));
        }
        for branch in ["", " ", "-x", "a..b", "a\\b", "a?b"] {
            assert!(!valid_ref(branch));
        }
    }
}
