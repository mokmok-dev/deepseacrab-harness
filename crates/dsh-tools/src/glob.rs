//! Find workspace files by glob pattern.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use glob::Pattern;
use serde::Deserialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

/// Find files under the session directory by glob.
pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "glob".to_string(),
            description: "Find files under the session directory by glob pattern.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: GlobArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args.pattern.trim().is_empty() {
            return ToolOutcome::error("glob pattern must not be empty");
        }
        let pattern = match Pattern::new(&args.pattern) {
            Ok(pattern) => pattern,
            Err(error) => return ToolOutcome::error(format!("invalid glob pattern: {error}")),
        };
        let root = match context.readable_path(args.path.as_deref().unwrap_or(".")) {
            Ok(root) => root,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        if !root.is_dir() {
            return ToolOutcome::error(format!("{} is not a directory", root.display()));
        }
        let mut matches = Vec::new();
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or_else(|_| entry.path());
            let relative = relative.to_string_lossy().replace('\\', "/");
            let name = entry.file_name().to_string_lossy();
            let is_match = if args.pattern.contains('/') {
                pattern.matches(&relative)
            } else {
                pattern.matches(&name)
            };
            if is_match {
                matches.push(relative);
                if matches.len() >= 500 {
                    break;
                }
            }
        }
        matches.sort();
        if matches.is_empty() {
            return ToolOutcome::text("no matches");
        }
        ToolOutcome::text(matches.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("a.rs"), "a").expect("a");
        std::fs::write(directory.path().join("b.txt"), "b").expect("b");
        std::fs::create_dir(directory.path().join("sub")).expect("sub");
        std::fs::write(directory.path().join("sub").join("c.rs"), "c").expect("c");
        directory
    }

    #[tokio::test]
    async fn patterns_match_file_names() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GlobTool.execute(&context, json!({"pattern": "*.rs"})).await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "a.rs\nsub/c.rs");
    }

    #[tokio::test]
    async fn patterns_with_slashes_match_relative_paths() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GlobTool
            .execute(&context, json!({"pattern": "sub/*.rs"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "sub/c.rs");
    }

    #[tokio::test]
    async fn missing_matches_are_not_errors() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GlobTool.execute(&context, json!({"pattern": "*.md"})).await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "no matches");
    }

    #[tokio::test]
    async fn invalid_patterns_and_paths_are_errors() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        assert!(
            GlobTool
                .execute(&context, json!({"pattern": ""}))
                .await
                .is_error
        );
        assert!(
            GlobTool
                .execute(&context, json!({"pattern": "["}))
                .await
                .is_error
        );
        assert!(
            GlobTool
                .execute(&context, json!({"pattern": "*", "path": "/etc"}))
                .await
                .is_error
        );
        assert!(
            GlobTool
                .execute(&context, json!({"pattern": "*", "path": "a.rs"}))
                .await
                .is_error
        );
    }
}
