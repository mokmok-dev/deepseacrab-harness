//! The `list_dir` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::path::Path;

const DEFAULT_PATH: &str = ".";

pub struct ListDirTool;

#[derive(Deserialize)]
struct ListDirArguments {
    path: Option<String>,
}

#[async_trait]
impl Tool for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".to_string(),
            description: "List one directory in the session workspace. Entries are sorted, \
                          directories end with `/`, and hidden entries are omitted unless the \
                          requested path names a hidden directory."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path, relative to the session working directory or absolute. Defaults to `.`."
                    }
                }
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: ListDirArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        let raw = arguments.path.as_deref().unwrap_or(DEFAULT_PATH);
        let path = match context.readable_path(raw) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let include_hidden = Path::new(raw)
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with('.'));

        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(entries) => entries,
            Err(error) => {
                return ToolOutcome::error(format!("cannot list {}: {error}", path.display()));
            }
        };

        let mut items: Vec<(String, bool)> = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !include_hidden && name.starts_with('.') {
                        continue;
                    }
                    let is_directory = match entry.file_type().await {
                        Ok(file_type) => file_type.is_dir(),
                        Err(error) => {
                            return ToolOutcome::error(format!(
                                "cannot list {}: {error}",
                                path.display()
                            ));
                        }
                    };
                    items.push((name, is_directory));
                }
                Ok(None) => break,
                Err(error) => {
                    return ToolOutcome::error(format!("cannot list {}: {error}", path.display()));
                }
            }
        }

        items.sort_by(|left, right| left.0.cmp(&right.0));
        let lines: Vec<String> = items
            .into_iter()
            .map(|(name, is_directory)| {
                if is_directory {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        ToolOutcome::text(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use std::fs;

    fn fixture(directory: &Path) {
        fs::write(directory.join("b.txt"), "").expect("write");
        fs::write(directory.join("a.txt"), "").expect("write");
        fs::write(directory.join(".hidden"), "").expect("write");
        fs::create_dir(directory.join("zdir")).expect("mkdir");
        fs::create_dir(directory.join("adir")).expect("mkdir");
        fs::create_dir(directory.join(".git")).expect("mkdir");
    }

    #[tokio::test]
    async fn entries_are_sorted_and_directories_are_suffixed() {
        let directory = tempfile::tempdir().expect("tempdir");
        fixture(directory.path());
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ListDirTool.execute(&context, json!({})).await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "a.txt\nadir/\nb.txt\nzdir/");
    }

    #[tokio::test]
    async fn hidden_directories_are_listed_when_named() {
        let directory = tempfile::tempdir().expect("tempdir");
        let hidden = directory.path().join(".config");
        fs::create_dir(&hidden).expect("mkdir");
        fs::write(hidden.join("settings.toml"), "").expect("write");
        fs::write(hidden.join(".secret"), "").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ListDirTool
            .execute(&context, json!({"path": ".config"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), ".secret\nsettings.toml");
    }

    #[tokio::test]
    async fn empty_directories_produce_empty_output() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("empty")).expect("mkdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ListDirTool
            .execute(&context, json!({"path": "empty"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "");
    }

    #[tokio::test]
    async fn missing_directories_are_tool_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ListDirTool
            .execute(&context, json!({"path": "absent"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("cannot list"));
    }

    #[tokio::test]
    async fn regular_files_are_not_directories() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("a.txt"), "").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ListDirTool
            .execute(&context, json!({"path": "a.txt"}))
            .await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::WorkspaceWrite,
        );
        let outcome = ListDirTool.execute(&context, json!({"path": ".."})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("outside workspace"));
    }
}
