//! The `read_file` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_LINE_LIMIT: usize = 2000;
const MAX_LINE_LIMIT: usize = 4000;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const NUMBER_WIDTH: usize = 6;

pub struct ReadFileTool;

#[derive(Deserialize)]
struct ReadFileArguments {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".to_string(),
            description:
                "Read a UTF-8 text file from the session workspace. Each returned line is \
                          prefixed with its right-aligned line number and a tab. Files larger \
                          than 1 MiB are refused."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the session working directory or absolute."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "First line to return, 1-based. Defaults to 1."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return. Defaults to 2000 and is capped at 4000."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: ReadFileArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        let path = match context.readable_path(&arguments.path) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
            }
        };
        if metadata.len() > MAX_FILE_BYTES {
            return ToolOutcome::error(format!(
                "file {} is too large ({} bytes; limit {MAX_FILE_BYTES} bytes)",
                path.display(),
                metadata.len()
            ));
        }

        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
            }
        };

        let offset = arguments.offset.unwrap_or(1);
        let limit = arguments
            .limit
            .unwrap_or(DEFAULT_LINE_LIMIT)
            .min(MAX_LINE_LIMIT);
        ToolOutcome::text(format_lines(&text, offset, limit))
    }
}

fn format_lines(text: &str, offset: usize, limit: usize) -> String {
    let start = offset.max(1);
    let lines: Vec<String> = text
        .lines()
        .skip(start - 1)
        .take(limit)
        .enumerate()
        .map(|(index, line)| {
            let number = start + index;
            format!("{number:>NUMBER_WIDTH$}\t{line}")
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use proptest::prelude::*;
    use std::fs;
    use std::path::Path;

    fn write_lines(directory: &Path, count: usize) -> std::path::PathBuf {
        let path = directory.join("lines.txt");
        let content = (1..=count)
            .map(|number| format!("line {number}"))
            .collect::<Vec<String>>()
            .join("\n");
        fs::write(&path, content).expect("write");
        path
    }

    #[tokio::test]
    async fn reads_number_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "a.txt"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "     1\talpha\n     2\tbeta");
    }

    #[tokio::test]
    async fn offset_and_limit_select_a_window() {
        let directory = tempfile::tempdir().expect("tempdir");
        write_lines(directory.path(), 10);
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(
                &context,
                json!({"path": "lines.txt", "offset": 2, "limit": 3}),
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "     2\tline 2\n     3\tline 3\n     4\tline 4"
        );
    }

    #[tokio::test]
    async fn default_limit_stops_at_2000_lines() {
        let directory = tempfile::tempdir().expect("tempdir");
        write_lines(directory.path(), 2500);
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "lines.txt"}))
            .await;
        let text = outcome.text_content();
        assert_eq!(text.lines().count(), 2000);
        assert!(text.ends_with("  2000\tline 2000"));
    }

    #[tokio::test]
    async fn requested_limits_are_capped_at_4000_lines() {
        let directory = tempfile::tempdir().expect("tempdir");
        write_lines(directory.path(), 4500);
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "lines.txt", "limit": 10_000}))
            .await;
        let text = outcome.text_content();
        assert_eq!(text.lines().count(), 4000);
        assert!(text.ends_with("  4000\tline 4000"));
    }

    #[tokio::test]
    async fn read_only_mode_still_allows_reads() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("a.txt"), "alpha").expect("write");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "a.txt"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "     1\talpha");
    }

    #[tokio::test]
    async fn missing_files_are_tool_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "absent.txt"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("absent.txt"));
    }

    #[tokio::test]
    async fn oversized_files_are_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let size = usize::try_from(MAX_FILE_BYTES).expect("fits") + 1;
        fs::write(directory.path().join("big.txt"), vec![b'a'; size]).expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "big.txt"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("too large"));
        assert!(outcome.text_content().contains("1048576"));
    }

    #[tokio::test]
    async fn files_at_the_size_limit_are_accepted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let size = usize::try_from(MAX_FILE_BYTES).expect("fits");
        fs::write(directory.path().join("limit.txt"), vec![b'a'; size]).expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "limit.txt", "limit": 1}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content().len(), NUMBER_WIDTH + 1 + size);
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        fs::write(root.path().join("secret.txt"), "hidden").expect("write");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::WorkspaceWrite,
        );
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "../secret.txt"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("outside workspace"));
    }

    #[tokio::test]
    async fn full_access_reads_outside_the_workspace() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        fs::write(root.path().join("secret.txt"), "hidden").expect("write");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::DangerFullAccess,
        );
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "../secret.txt"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "     1\thidden");
    }

    #[tokio::test]
    async fn invalid_utf8_is_reported() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("binary.txt"), [0xFF_u8, 0xFE]).expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = ReadFileTool
            .execute(&context, json!({"path": "binary.txt"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("cannot read"));
    }

    #[test]
    fn empty_ranges_produce_empty_output() {
        assert_eq!(format_lines("a\nb\nc", 10, 5), "");
        assert_eq!(format_lines("a\nb\nc", 1, 0), "");
        assert_eq!(format_lines("", 1, 5), "");
    }

    #[test]
    fn offsets_below_one_start_at_the_first_line() {
        assert_eq!(format_lines("a\nb", 0, 5), "     1\ta\n     2\tb");
    }

    proptest! {
        #[test]
        fn line_numbers_increase_by_one(
            lines in prop::collection::vec("[a-z ]{0,8}", 0..20),
            offset in 1usize..5,
            limit in 0usize..10,
        ) {
            let text = lines.join("\n");
            let output = format_lines(&text, offset, limit);
            for (expected, line) in (offset.max(1)..).zip(output.lines()) {
                let (prefix, content) = line.split_once('\t').expect("tab separator");
                let number: usize = prefix.trim().parse().expect("line number");
                prop_assert_eq!(number, expected);
                prop_assert_eq!(prefix.len(), NUMBER_WIDTH);
                prop_assert!(!content.contains('\t'));
            }
            prop_assert!(output.lines().count() <= limit);
        }
    }
}
