//! Load a workspace skill by name.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

/// Load `SKILL.md` from a workspace skill directory.
pub struct SkillTool;

#[derive(Deserialize)]
struct SkillArgs {
    name: String,
}

const MAX_SKILL_BYTES: u64 = 65_536;

#[async_trait]
impl Tool for SkillTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "skill".to_string(),
            description: "Load a workspace skill's SKILL.md instructions by name.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: SkillArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if !valid_name(&args.name) {
            return ToolOutcome::error("skill name must be a simple directory name");
        }
        for root in [".skills", ".grok/skills"] {
            let relative = format!("{root}/{}/SKILL.md", args.name);
            let Ok(path) = context.readable_path(&relative) else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            if let Ok(metadata) = fs::metadata(&path) {
                if metadata.len() > MAX_SKILL_BYTES {
                    return ToolOutcome::error(format!(
                        "skill {} is larger than {} bytes",
                        args.name, MAX_SKILL_BYTES
                    ));
                }
            }
            return match fs::read_to_string(&path) {
                Ok(body) => ToolOutcome::text(body),
                Err(error) => {
                    ToolOutcome::error(format!("cannot read {}: {error}", path.display()))
                }
            };
        }
        let available = available_skills(&context.cwd);
        if available.is_empty() {
            ToolOutcome::error(format!("unknown skill: {}", args.name))
        } else {
            ToolOutcome::error(format!(
                "unknown skill: {}. available: {}",
                args.name,
                available.join(", ")
            ))
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.chars().any(char::is_control)
}

fn available_skills(cwd: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for root in [".skills", ".grok/skills"] {
        let Ok(entries) = fs::read_dir(cwd.join(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("SKILL.md").is_file() {
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;

    #[tokio::test]
    async fn loads_a_workspace_skill() {
        let directory = tempfile::tempdir().expect("tempdir");
        let skill = directory.path().join(".skills").join("demo");
        std::fs::create_dir_all(&skill).expect("skill dir");
        std::fs::write(skill.join("SKILL.md"), "# Demo\nUse it.").expect("skill");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = SkillTool.execute(&context, json!({"name": "demo"})).await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "# Demo\nUse it.");
    }

    #[tokio::test]
    async fn missing_skills_list_available_names() {
        let directory = tempfile::tempdir().expect("tempdir");
        let skill = directory.path().join(".grok").join("skills").join("alpha");
        std::fs::create_dir_all(&skill).expect("skill dir");
        std::fs::write(skill.join("SKILL.md"), "alpha").expect("skill");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = SkillTool
            .execute(&context, json!({"name": "missing"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("alpha"));
    }

    #[tokio::test]
    async fn invalid_names_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        for name in ["", "../escape", "a/b", "a\\b", "line\nbreak"] {
            let outcome = SkillTool.execute(&context, json!({"name": name})).await;
            assert!(outcome.is_error);
            assert!(outcome.text_content().contains("simple directory name"));
        }
    }

    #[tokio::test]
    async fn skill_size_limit_is_inclusive() {
        let directory = tempfile::tempdir().expect("tempdir");
        let skill = directory.path().join(".skills").join("large");
        std::fs::create_dir_all(&skill).expect("skill dir");
        let exact = "a".repeat(usize::try_from(MAX_SKILL_BYTES).expect("limit"));
        std::fs::write(skill.join("SKILL.md"), &exact).expect("exact");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = SkillTool.execute(&context, json!({"name": "large"})).await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content().len(), exact.len());

        let oversized = "a".repeat(usize::try_from(MAX_SKILL_BYTES).expect("limit") + 1);
        std::fs::write(skill.join("SKILL.md"), oversized).expect("oversized");
        let outcome = SkillTool.execute(&context, json!({"name": "large"})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("larger than"));
    }

    #[tokio::test]
    async fn no_skills_directory_reports_unknown() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = SkillTool
            .execute(&context, json!({"name": "missing"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("unknown skill"));
    }
}
