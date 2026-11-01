//! The `write_file` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct WriteFileTool;

#[derive(Deserialize)]
struct WriteFileArguments {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".to_string(),
            description: "Write UTF-8 text to a file in the session workspace, creating missing \
                          parent directories. Refused in read-only mode and outside the workspace."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the session working directory or absolute."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full UTF-8 content to write."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: WriteFileArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        let path = match context.writable_path(&arguments.path) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };

        if let Some(parent) = path.parent() {
            if let Err(error) = tokio::fs::create_dir_all(parent).await {
                return ToolOutcome::error(format!(
                    "cannot create parent directory for {}: {error}",
                    path.display()
                ));
            }
        }

        if let Err(error) = tokio::fs::write(&path, &arguments.content).await {
            return ToolOutcome::error(format!("cannot write {}: {error}", path.display()));
        }

        ToolOutcome::text(format!(
            "wrote {} bytes to {}",
            arguments.content.len(),
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use std::fs;

    #[tokio::test]
    async fn writes_files_and_creates_parents() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WriteFileTool
            .execute(
                &context,
                json!({"path": "nested/deeper/note.txt", "content": "hello"}),
            )
            .await;
        assert!(!outcome.is_error);
        let written = directory.path().join("nested/deeper/note.txt");
        assert_eq!(fs::read_to_string(&written).expect("read"), "hello");
        assert_eq!(
            outcome.text_content(),
            format!("wrote 5 bytes to {}", written.display())
        );
    }

    #[tokio::test]
    async fn byte_counts_use_utf8_length() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WriteFileTool
            .execute(&context, json!({"path": "u.txt", "content": "é"}))
            .await;
        assert!(outcome.text_content().starts_with("wrote 2 bytes to "));
    }

    #[tokio::test]
    async fn overwrites_existing_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("note.txt"), "old").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WriteFileTool
            .execute(&context, json!({"path": "note.txt", "content": "new"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).expect("read"),
            "new"
        );
    }

    #[tokio::test]
    async fn read_only_mode_refuses_writes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = WriteFileTool
            .execute(&context, json!({"path": "note.txt", "content": "no"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
        assert!(!directory.path().join("note.txt").exists());
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::WorkspaceWrite,
        );
        let outcome = WriteFileTool
            .execute(&context, json!({"path": "../escape.txt", "content": "no"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("outside workspace"));
        assert!(!root.path().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn full_access_writes_outside_the_workspace() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::DangerFullAccess,
        );
        let outcome = WriteFileTool
            .execute(&context, json!({"path": "../escape.txt", "content": "yes"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            fs::read_to_string(root.path().join("escape.txt")).expect("read"),
            "yes"
        );
    }

    #[tokio::test]
    async fn missing_content_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WriteFileTool
            .execute(&context, json!({"path": "note.txt"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));
    }
}
