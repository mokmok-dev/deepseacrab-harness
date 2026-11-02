//! Read or change the session working directory.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{SessionEvent, Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};

/// Read or change the session working directory.
pub struct WorkingDirectoryTool;

#[derive(Deserialize)]
struct DirectoryArgs {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for WorkingDirectoryTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "working_directory".to_string(),
            description:
                "Read the session working directory, or change it to an existing directory."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: DirectoryArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let Some(raw) = args.path else {
            return ToolOutcome::text(context.cwd.display().to_string());
        };
        let path = match context.readable_path(&raw) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                return ToolOutcome::error(format!(
                    "cannot resolve directory {}: {error}",
                    path.display()
                ));
            }
        };
        if !canonical.is_dir() {
            return ToolOutcome::error(format!("{} is not a directory", canonical.display()));
        }
        let Some(session) = context.session() else {
            return ToolOutcome::error("this tool requires a durable session");
        };
        if let Err(error) = session.append(SessionEvent::WorkingDirectoryChanged {
            cwd: canonical.display().to_string(),
        }) {
            return ToolOutcome::error(error.to_string());
        }
        ToolOutcome::text(canonical.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::session_context;
    use dsh_core::PermissionMode;

    #[tokio::test]
    async fn no_argument_reports_the_current_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (context, _session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WorkingDirectoryTool.execute(&context, json!({})).await;
        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            directory.path().display().to_string()
        );
    }

    #[tokio::test]
    async fn changing_the_directory_records_a_durable_fact() {
        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("nested");
        std::fs::create_dir(&target).expect("nested");
        let (context, session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WorkingDirectoryTool
            .execute(&context, json!({"path": "nested"}))
            .await;
        assert!(!outcome.is_error);
        let canonical = target.canonicalize().expect("canonical");
        assert_eq!(outcome.text_content(), canonical.display().to_string());
        assert!(matches!(
            &session.events()[0],
            SessionEvent::WorkingDirectoryChanged { cwd } if *cwd == canonical.display().to_string()
        ));
    }

    #[tokio::test]
    async fn missing_directories_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (context, _session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WorkingDirectoryTool
            .execute(&context, json!({"path": "missing"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("cannot resolve"));
    }

    #[tokio::test]
    async fn files_are_not_directories() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "x").expect("file");
        let (context, _session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WorkingDirectoryTool
            .execute(&context, json!({"path": "file.txt"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("not a directory"));
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (context, _session) = session_context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WorkingDirectoryTool
            .execute(&context, json!({"path": "/etc"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("outside workspace"));
    }

    #[tokio::test]
    async fn a_session_is_required_for_changes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("nested");
        std::fs::create_dir(&target).expect("nested");
        let context = crate::test_util::context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WorkingDirectoryTool
            .execute(&context, json!({"path": "nested"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("durable session"));
    }
}
