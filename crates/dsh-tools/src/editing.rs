//! The `edit_file` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct EditFileTool;

#[derive(Deserialize)]
struct EditFileArguments {
    path: String,
    old: String,
    new: String,
    replace_all: Option<bool>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit_file".to_string(),
            description: "Replace exact text in a UTF-8 file. Replaces the first occurrence by \
                          default, or every occurrence when `replace_all` is true. Refused in \
                          read-only mode and outside the workspace."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the session working directory or absolute."
                    },
                    "old": {
                        "type": "string",
                        "description": "Exact text to find. Must not be empty."
                    },
                    "new": {
                        "type": "string",
                        "description": "Replacement text."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of only the first. Defaults to false."
                    }
                },
                "required": ["path", "old", "new"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: EditFileArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        if arguments.old.is_empty() {
            return ToolOutcome::error("old text must not be empty");
        }
        let path = match context.writable_path(&arguments.path) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };

        let original = match tokio::fs::read_to_string(&path).await {
            Ok(original) => original,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
            }
        };
        let replace_all = arguments.replace_all.unwrap_or(false);
        let Some((updated, replaced)) =
            apply_edit(&original, &arguments.old, &arguments.new, replace_all)
        else {
            return ToolOutcome::error(format!("no match for old text in {}", path.display()));
        };

        if let Err(error) = tokio::fs::write(&path, &updated).await {
            return ToolOutcome::error(format!("cannot write {}: {error}", path.display()));
        }
        ToolOutcome::text(format!("replaced {replaced} occurrence(s)"))
    }
}

fn apply_edit(content: &str, old: &str, new: &str, replace_all: bool) -> Option<(String, usize)> {
    if old.is_empty() {
        return None;
    }
    let occurrences = content.matches(old).count();
    if occurrences == 0 {
        return None;
    }
    if replace_all {
        Some((content.replace(old, new), occurrences))
    } else {
        Some((content.replacen(old, new, 1), 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use proptest::prelude::*;
    use std::fs;

    fn disjoint_old() -> impl Strategy<Value = String> {
        "[a-c]{1,4}"
    }

    fn disjoint_new() -> impl Strategy<Value = String> {
        "[x-z0-9]{0,4}"
    }

    fn disjoint_content() -> impl Strategy<Value = String> {
        "[a-cx-z0-9 ]{0,40}"
    }

    #[tokio::test]
    async fn replaces_the_first_occurrence_by_default() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("note.txt"), "one two one").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = EditFileTool
            .execute(
                &context,
                json!({"path": "note.txt", "old": "one", "new": "three"}),
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "replaced 1 occurrence(s)");
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).expect("read"),
            "three two one"
        );
    }

    #[tokio::test]
    async fn replaces_every_occurrence_when_asked() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("note.txt"), "one two one").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = EditFileTool
            .execute(
                &context,
                json!({"path": "note.txt", "old": "one", "new": "three", "replace_all": true}),
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "replaced 2 occurrence(s)");
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).expect("read"),
            "three two three"
        );
    }

    #[tokio::test]
    async fn missing_matches_name_the_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("note.txt"), "one").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = EditFileTool
            .execute(
                &context,
                json!({"path": "note.txt", "old": "absent", "new": "x"}),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("note.txt"));
        assert!(outcome.text_content().contains("no match"));
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).expect("read"),
            "one"
        );
    }

    #[tokio::test]
    async fn empty_old_text_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("note.txt"), "one").expect("write");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = EditFileTool
            .execute(&context, json!({"path": "note.txt", "old": "", "new": "x"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("must not be empty"));
    }

    #[tokio::test]
    async fn read_only_mode_refuses_edits() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("note.txt"), "one").expect("write");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = EditFileTool
            .execute(
                &context,
                json!({"path": "note.txt", "old": "one", "new": "x"}),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).expect("read"),
            "one"
        );
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir(root.path().join("workspace")).expect("mkdir");
        fs::write(root.path().join("outside.txt"), "one").expect("write");
        let context = context(
            &root.path().join("workspace"),
            PermissionMode::WorkspaceWrite,
        );
        let outcome = EditFileTool
            .execute(
                &context,
                json!({"path": "../outside.txt", "old": "one", "new": "x"}),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("outside workspace"));
    }

    #[test]
    fn empty_old_text_and_absent_text_yield_nothing() {
        assert_eq!(apply_edit("abc", "", "x", false), None);
        assert_eq!(apply_edit("abc", "z", "x", true), None);
        assert_eq!(apply_edit("", "a", "x", true), None);
    }

    #[test]
    fn replace_all_counts_every_non_overlapping_match() {
        assert_eq!(
            apply_edit("aaaa", "aa", "b", true),
            Some(("bb".to_string(), 2))
        );
        assert_eq!(
            apply_edit("aaaa", "aa", "b", false),
            Some(("baa".to_string(), 1))
        );
    }

    proptest! {
        #[test]
        fn replace_all_removes_disjoint_matches(
            content in disjoint_content(),
            old in disjoint_old(),
            new in disjoint_new(),
        ) {
            let occurrences = content.matches(old.as_str()).count();
            let result = apply_edit(&content, &old, &new, true);
            if occurrences == 0 {
                prop_assert!(result.is_none());
            } else {
                let (updated, replaced) = result.expect("match");
                prop_assert_eq!(replaced, occurrences);
                prop_assert_eq!(&updated, &content.replace(old.as_str(), new.as_str()));
                prop_assert_eq!(
                    updated.len() + replaced * old.len(),
                    content.len() + replaced * new.len()
                );
            }
        }

        #[test]
        fn replaced_once_rewrites_only_the_first_match(
            content in disjoint_content(),
            old in disjoint_old(),
            new in disjoint_new(),
        ) {
            let occurrences = content.matches(old.as_str()).count();
            let result = apply_edit(&content, &old, &new, false);
            if occurrences == 0 {
                prop_assert!(result.is_none());
            } else {
                let (updated, replaced) = result.expect("match");
                prop_assert_eq!(replaced, 1);
                prop_assert_eq!(&updated, &content.replacen(old.as_str(), new.as_str(), 1));
                let first = content.find(old.as_str()).expect("match position");
                let expected = format!(
                    "{}{}{}",
                    &content[..first],
                    new,
                    &content[first + old.len()..]
                );
                prop_assert_eq!(updated, expected);
            }
        }

        #[test]
        fn arbitrary_text_matches_standard_replacement(
            content in any::<String>(),
            old in any::<String>(),
            new in any::<String>(),
        ) {
            if old.is_empty() {
                prop_assert!(apply_edit(&content, &old, &new, true).is_none());
                prop_assert!(apply_edit(&content, &old, &new, false).is_none());
            } else {
                let occurrences = content.matches(old.as_str()).count();
                match apply_edit(&content, &old, &new, true) {
                    None => prop_assert_eq!(occurrences, 0),
                    Some((updated, replaced)) => {
                        prop_assert_eq!(replaced, occurrences);
                        prop_assert_eq!(&updated, &content.replace(old.as_str(), new.as_str()));
                    }
                }
                match apply_edit(&content, &old, &new, false) {
                    None => prop_assert_eq!(occurrences, 0),
                    Some((updated, replaced)) => {
                        prop_assert_eq!(replaced, 1);
                        prop_assert_eq!(&updated, &content.replacen(old.as_str(), new.as_str(), 1));
                    }
                }
            }
        }
    }
}
