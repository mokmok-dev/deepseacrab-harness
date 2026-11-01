//! The `grep` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

const DEFAULT_PATH: &str = ".";
const MAX_MATCHES: usize = 200;
const BINARY_SNIFF_BYTES: usize = 8192;

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArguments {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
}

#[async_trait]
impl Tool for GrepTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".to_string(),
            description: "Search file contents in the session workspace with a regular \
                          expression. Prints `relative/path:line: text` per matching line, up to \
                          200 matches. Skips `.git`, `target`, `node_modules`, and binary files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Rust `regex` pattern matched against each line."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search. Defaults to `.`."
                    },
                    "glob": {
                        "type": "string",
                        "description": "File name filter such as `*.rs` or `*.ts`."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: GrepArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        let source = arguments.pattern;
        let pattern = match Regex::new(&source) {
            Ok(pattern) => pattern,
            Err(error) => {
                return ToolOutcome::error(format!("invalid pattern {source:?}: {error}"));
            }
        };
        let raw = arguments.path.as_deref().unwrap_or(DEFAULT_PATH);
        let root = match context.readable_path(raw) {
            Ok(root) => root,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        if let Err(error) = tokio::fs::metadata(&root).await {
            return ToolOutcome::error(format!("cannot search {}: {error}", root.display()));
        }

        let mut matches = search(&root, &context.cwd, &pattern, arguments.glob.as_deref());
        matches.sort();
        ToolOutcome::text(matches.join("\n"))
    }
}

fn search(root: &Path, cwd: &Path, pattern: &Regex, glob: Option<&str>) -> Vec<String> {
    let mut matches = Vec::new();
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_skipped(entry));

    'walk: for entry in walker {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if let Some(glob) = glob {
            if !glob_matches(glob, &file_name) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let relative = entry.path().strip_prefix(cwd).unwrap_or(entry.path());
        for (index, line) in text.lines().enumerate() {
            if pattern.is_match(line) {
                matches.push(format!("{}:{}: {line}", relative.display(), index + 1));
                if matches.len() == MAX_MATCHES {
                    break 'walk;
                }
            }
        }
    }
    matches
}

fn is_skipped(entry: &DirEntry) -> bool {
    matches!(
        entry.file_name().to_str(),
        Some(".git" | "target" | "node_modules")
    )
}

fn glob_matches(pattern: &str, file_name: &str) -> bool {
    pattern.strip_prefix('*').map_or_else(
        || file_name == pattern,
        |suffix| file_name.ends_with(suffix),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use std::fs;
    use std::path::Path;

    fn write(directory: &Path, relative: &str, content: &str) {
        let path = directory.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, content).expect("write");
    }

    fn fixture(directory: &Path) {
        write(directory, "a.rs", "let alpha = 1;\nlet beta = 2;\n");
        write(directory, "b.ts", "alpha\n");
        write(directory, "src/nested.rs", "alpha nested\n");
    }

    #[tokio::test]
    async fn matches_are_sorted_and_report_line_numbers() {
        let directory = tempfile::tempdir().expect("tempdir");
        fixture(directory.path());
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "alpha"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "a.rs:1: let alpha = 1;\nb.ts:1: alpha\nsrc/nested.rs:1: alpha nested"
        );
    }

    #[tokio::test]
    async fn line_numbers_count_from_one() {
        let directory = tempfile::tempdir().expect("tempdir");
        fixture(directory.path());
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool.execute(&context, json!({"pattern": "beta"})).await;
        assert_eq!(outcome.text_content(), "a.rs:2: let beta = 2;");
    }

    #[tokio::test]
    async fn globs_filter_by_file_name_suffix() {
        let directory = tempfile::tempdir().expect("tempdir");
        fixture(directory.path());
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "alpha", "glob": "*.rs"}))
            .await;
        assert_eq!(
            outcome.text_content(),
            "a.rs:1: let alpha = 1;\nsrc/nested.rs:1: alpha nested"
        );

        let exact = GrepTool
            .execute(&context, json!({"pattern": "alpha", "glob": "b.ts"}))
            .await;
        assert_eq!(exact.text_content(), "b.ts:1: alpha");
    }

    #[tokio::test]
    async fn paths_are_relative_to_the_working_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        fixture(directory.path());
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "nested", "path": "src"}))
            .await;
        assert_eq!(outcome.text_content(), "src/nested.rs:1: alpha nested");
    }

    #[tokio::test]
    async fn build_and_dependency_directories_are_skipped() {
        let directory = tempfile::tempdir().expect("tempdir");
        write(directory.path(), "target/x.rs", "needle\n");
        write(directory.path(), ".git/y.rs", "needle\n");
        write(directory.path(), "node_modules/z.js", "needle\n");
        write(directory.path(), "src/ok.rs", "needle\n");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "needle"}))
            .await;
        assert_eq!(outcome.text_content(), "src/ok.rs:1: needle");
    }

    #[tokio::test]
    async fn binary_and_non_utf8_files_are_skipped() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(
            directory.path().join("binary.bin"),
            [b"needle".as_slice(), &[0_u8]].concat(),
        )
        .expect("write");
        fs::write(directory.path().join("latin1.txt"), [0xFF_u8, 0xFE]).expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "needle"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "");
    }

    #[tokio::test]
    async fn matches_are_capped_at_200() {
        let directory = tempfile::tempdir().expect("tempdir");
        let body = (0..250)
            .map(|index| format!("match {index}"))
            .collect::<Vec<String>>()
            .join("\n");
        write(directory.path(), "many.txt", &body);
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "match"}))
            .await;
        let text = outcome.text_content();
        assert_eq!(text.lines().count(), MAX_MATCHES);
        assert!(text.contains("many.txt:200: match 199"));
        assert_eq!(MAX_MATCHES, 200);
    }

    #[tokio::test]
    async fn no_matches_are_not_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        fixture(directory.path());
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "absent"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "");
    }

    #[tokio::test]
    async fn invalid_patterns_are_tool_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool.execute(&context, json!({"pattern": "^("})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid pattern"));
    }

    #[tokio::test]
    async fn missing_paths_are_tool_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "x", "path": "absent"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("cannot search"));
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::WorkspaceWrite,
        );
        let outcome = GrepTool
            .execute(&context, json!({"pattern": "x", "path": ".."}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("outside workspace"));
    }

    #[test]
    fn binary_sniffing_covers_eight_kibibytes() {
        assert_eq!(BINARY_SNIFF_BYTES, 8192);
    }

    #[test]
    fn glob_matching_handles_suffixes_and_exact_names() {
        assert!(glob_matches("*.rs", "main.rs"));
        assert!(!glob_matches("*.rs", "main.ts"));
        assert!(glob_matches("main.rs", "main.rs"));
        assert!(!glob_matches("main.rs", "other.rs"));
        assert!(glob_matches("*", "anything"));
    }
}
