//! The `job_kill` tool.

use crate::files::{JOB_DIR, JobPaths, read_pid, valid_job_id};
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Command;

pub struct JobKillTool;

#[derive(Deserialize)]
struct KillArguments {
    id: String,
}

#[async_trait]
impl Tool for JobKillTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "job_kill".to_string(),
            description:
                "Kill a background job with `kill -9` and return `killed <id>`. Fails when \
                          the job has no recorded process id."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Job id returned by job_start."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: KillArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if !valid_job_id(&arguments.id) {
            return ToolOutcome::error(format!("invalid job id: {}", arguments.id));
        }
        let directory = match context.readable_path(JOB_DIR) {
            Ok(directory) => directory,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let Some(pid) = read_pid(&JobPaths::new(&directory, &arguments.id).pid()) else {
            return ToolOutcome::error(format!("no pid file for job {}", arguments.id));
        };
        if let Err(error) = Command::new("kill").arg("-9").arg(pid.to_string()).status() {
            return ToolOutcome::error(format!("cannot kill job {}: {error}", arguments.id));
        }
        ToolOutcome::text(format!("killed {}", arguments.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{context, start_job, wait_for_status};
    use dsh_core::PermissionMode;
    use std::path::Path;

    #[tokio::test]
    async fn killing_a_running_job_lets_the_waiter_record_its_end() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, pid) = start_job(&context, "sleep 5", None).await;

        let outcome = JobKillTool.execute(&context, json!({"id": &id})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), format!("killed {id}"));
        assert_eq!(wait_for_status(&context, &id).await, "signal");
        assert!(!Path::new("/proc").join(pid.to_string()).exists());
    }

    #[tokio::test]
    async fn a_missing_pid_file_is_a_tool_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = JobKillTool
            .execute(&context, json!({"id": "deadbeef"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("no pid file"));
    }

    #[tokio::test]
    async fn path_shaped_ids_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        for id in ["", "..", "../../etc/passwd", "a/b", "a\\b", "a.b"] {
            let outcome = JobKillTool.execute(&context, json!({"id": id})).await;
            assert!(outcome.is_error, "{id:?}");
            assert!(
                outcome.text_content().contains("invalid job id"),
                "{id:?}: {}",
                outcome.text_content()
            );
        }
    }

    #[tokio::test]
    async fn missing_arguments_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = JobKillTool.execute(&context, json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));
    }
}
