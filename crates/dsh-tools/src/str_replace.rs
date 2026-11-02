//! View, create, replace, and insert into text files.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;

/// Text editor with view, create, replace, and insert commands.
pub struct StrReplaceEditorTool;

#[derive(Deserialize)]
struct EditorArgs {
    command: String,
    path: String,
    #[serde(default)]
    old_str: Option<String>,
    #[serde(default)]
    new_str: Option<String>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    insert_line: Option<usize>,
    #[serde(default)]
    view_range: Option<Vec<usize>>,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl Tool for StrReplaceEditorTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "str_replace_editor".to_string(),
            description:
                "View, create, replace exact text in, or insert lines into a workspace file."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert"]},
                    "path": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "replace_all": {"type": "boolean"},
                    "insert_line": {"type": "integer", "minimum": 0},
                    "view_range": {"type": "array", "items": {"type": "integer", "minimum": 1}},
                    "content": {"type": "string"}
                },
                "required": ["command", "path"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: EditorArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        match args.command.as_str() {
            "view" => view(context, &args),
            "create" => create(context, &args),
            "str_replace" => replace(context, &args),
            "insert" => insert(context, &args),
            other => ToolOutcome::error(format!("unsupported editor command: {other}")),
        }
    }
}

fn view(context: &ToolContext, args: &EditorArgs) -> ToolOutcome {
    let path = match context.readable_path(&args.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error.to_string()),
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
        }
    };
    if text.len() > 1_048_576 {
        return ToolOutcome::error("file is too large to view");
    }
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = match args.view_range.as_deref() {
        Some([start, end]) if *start >= 1 && start <= end && *end <= lines.len() => {
            (*start - 1, *end)
        }
        Some(_) => return ToolOutcome::error("view_range is outside the file"),
        None => (0, lines.len().min(4000)),
    };
    let mut output = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        output.push_str(&format!("{:>6}\t{line}\n", start + index + 1));
    }
    ToolOutcome::text(output)
}

fn create(context: &ToolContext, args: &EditorArgs) -> ToolOutcome {
    let path = match context.writable_path(&args.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error.to_string()),
    };
    let Some(body) = args.content.as_deref() else {
        return ToolOutcome::error("create requires content");
    };
    if path.exists() {
        return ToolOutcome::error(format!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return ToolOutcome::error(format!("cannot create {}: {error}", parent.display()));
        }
    }
    match fs::write(&path, body) {
        Ok(()) => ToolOutcome::text(format!("created {}", path.display())),
        Err(error) => ToolOutcome::error(format!("cannot write {}: {error}", path.display())),
    }
}

fn replace(context: &ToolContext, args: &EditorArgs) -> ToolOutcome {
    let path = match context.writable_path(&args.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error.to_string()),
    };
    let Some(old) = args.old_str.as_deref() else {
        return ToolOutcome::error("str_replace requires old_str");
    };
    if old.is_empty() {
        return ToolOutcome::error("old_str must not be empty");
    }
    let new = args.new_str.as_deref().unwrap_or("");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
        }
    };
    let count = text.matches(old).count();
    if count == 0 {
        return ToolOutcome::error(format!("old_str was not found in {}", path.display()));
    }
    let updated = if args.replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    match fs::write(&path, updated) {
        Ok(()) => ToolOutcome::text(format!(
            "replaced {} occurrence(s) in {}",
            if args.replace_all { count } else { 1 },
            path.display()
        )),
        Err(error) => ToolOutcome::error(format!("cannot write {}: {error}", path.display())),
    }
}

fn insert(context: &ToolContext, args: &EditorArgs) -> ToolOutcome {
    let path = match context.writable_path(&args.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error.to_string()),
    };
    let Some(new) = args.new_str.as_deref() else {
        return ToolOutcome::error("insert requires new_str");
    };
    let Some(line) = args.insert_line else {
        return ToolOutcome::error("insert requires insert_line");
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
        }
    };
    let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
    if line > lines.len() {
        return ToolOutcome::error("insert_line is outside the file");
    }
    lines.insert(line, new.to_string());
    let mut updated = lines.join("\n");
    if text.ends_with('\n') {
        updated.push('\n');
    }
    match fs::write(&path, updated) {
        Ok(()) => ToolOutcome::text(format!("inserted at line {line} in {}", path.display())),
        Err(error) => ToolOutcome::error(format!("cannot write {}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "one\ntwo\nthree\n").expect("file");
        directory
    }

    #[tokio::test]
    async fn view_numbers_lines_and_honors_ranges() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let all = StrReplaceEditorTool
            .execute(&context, json!({"command": "view", "path": "file.txt"}))
            .await;
        assert_eq!(
            all.text_content(),
            "     1\tone\n     2\ttwo\n     3\tthree\n"
        );
        let range = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "view", "path": "file.txt", "view_range": [2, 3]}),
            )
            .await;
        assert_eq!(range.text_content(), "     2\ttwo\n     3\tthree\n");
        let single = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "view", "path": "file.txt", "view_range": [1, 1]}),
            )
            .await;
        assert_eq!(single.text_content(), "     1\tone\n");
        for range in [[0, 2], [2, 1], [1, 4], [4, 4]] {
            assert!(
                StrReplaceEditorTool
                    .execute(
                        &context,
                        json!({"command": "view", "path": "file.txt", "view_range": range})
                    )
                    .await
                    .is_error
            );
        }
    }

    #[tokio::test]
    async fn create_writes_new_files_and_rejects_existing_ones() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let created = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "create", "path": "nested/new.txt", "content": "new"}),
            )
            .await;
        assert!(!created.is_error);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("nested/new.txt")).expect("read"),
            "new"
        );
        assert!(
            StrReplaceEditorTool
                .execute(
                    &context,
                    json!({"command": "create", "path": "nested/new.txt", "content": "again"})
                )
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn replace_handles_once_and_all() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let once = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "str_replace", "path": "file.txt", "old_str": "t", "new_str": "T"}),
            )
            .await;
        assert!(!once.is_error);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).expect("read"),
            "one\nTwo\nthree\n"
        );
        let all = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "str_replace", "path": "file.txt", "old_str": "e", "new_str": "E", "replace_all": true}),
            )
            .await;
        assert!(!all.is_error);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).expect("read"),
            "onE\nTwo\nthrEE\n"
        );
    }

    #[tokio::test]
    async fn replace_rejects_missing_and_empty_old_text() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        assert!(
            StrReplaceEditorTool
                .execute(
                    &context,
                    json!({"command": "str_replace", "path": "file.txt", "old_str": "absent", "new_str": "x"})
                )
                .await
                .is_error
        );
        assert!(
            StrReplaceEditorTool
                .execute(
                    &context,
                    json!({"command": "str_replace", "path": "file.txt", "old_str": "", "new_str": "x"})
                )
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn insert_places_lines_at_the_requested_position() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        for (line, expected) in [
            (0, "zero\none\ntwo\nthree\n"),
            (2, "one\ntwo\nmid\nthree\n"),
            (3, "one\ntwo\nthree\nend\n"),
        ] {
            std::fs::write(directory.path().join("file.txt"), "one\ntwo\nthree\n").expect("reset");
            let outcome = StrReplaceEditorTool
                .execute(
                    &context,
                    json!({"command": "insert", "path": "file.txt", "insert_line": line, "new_str": if line == 0 { "zero" } else if line == 2 { "mid" } else { "end" }}),
                )
                .await;
            assert!(!outcome.is_error);
            assert_eq!(
                std::fs::read_to_string(directory.path().join("file.txt")).expect("read"),
                expected
            );
        }
        assert!(
            StrReplaceEditorTool
                .execute(
                    &context,
                    json!({"command": "insert", "path": "file.txt", "insert_line": 99, "new_str": "x"})
                )
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn view_size_limit_is_inclusive() {
        let directory = tempfile::tempdir().expect("tempdir");
        let exact = "a".repeat(1_048_576);
        std::fs::write(directory.path().join("exact.txt"), &exact).expect("exact");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "view", "path": "exact.txt", "view_range": [1, 1]}),
            )
            .await;
        assert!(!outcome.is_error);

        let oversized = "a".repeat(1_048_577);
        std::fs::write(directory.path().join("large.txt"), oversized).expect("large");
        let outcome = StrReplaceEditorTool
            .execute(
                &context,
                json!({"command": "view", "path": "large.txt", "view_range": [1, 1]}),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("too large"));
    }

    #[tokio::test]
    async fn unknown_commands_and_bad_paths_are_errors() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        assert!(
            StrReplaceEditorTool
                .execute(&context, json!({"command": "delete", "path": "file.txt"}))
                .await
                .is_error
        );
        assert!(
            StrReplaceEditorTool
                .execute(&context, json!({"command": "view", "path": "../outside"}))
                .await
                .is_error
        );
    }
}
