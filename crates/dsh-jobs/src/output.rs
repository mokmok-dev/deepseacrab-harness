//! The `job_output` tool.

use crate::files::{JOB_DIR, JobPaths, read_pid, valid_job_id};
use crate::stream::{Stream, format_output, read_stream};
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct JobOutputTool;

#[derive(Deserialize)]
struct OutputArguments {
    id: String,
    stream: Option<Stream>,
}

#[async_trait]
impl Tool for JobOutputTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "job_output".to_string(),
            description: "Read a background job's captured output. Returns the requested stream, \
                          or both streams labelled `stdout:` and `stderr:`. Each stream is capped \
                          at 64 KiB."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Job id returned by job_start."
                    },
                    "stream": {
                        "type": "string",
                        "enum": ["stdout", "stderr"],
                        "description": "Stream to return. Omit to return both."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: OutputArguments = match serde_json::from_value(arguments) {
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
        let paths = JobPaths::new(&directory, &arguments.id);
        if read_pid(&paths.pid()).is_none() {
            return ToolOutcome::error(format!("unknown job: {}", arguments.id));
        }
        let stdout = match read_stream(&paths.stdout()) {
            Ok(stdout) => stdout,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read job output: {error}"));
            }
        };
        let stderr = match read_stream(&paths.stderr()) {
            Ok(stderr) => stderr,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read job output: {error}"));
            }
        };
        ToolOutcome::text(format_output(&stdout, &stderr, arguments.stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::PID_SUFFIX;
    use crate::stream::MAX_STREAM_BYTES;
    use crate::test_util::{context, job_directory, start_job, wait_for_status};
    use dsh_core::{PermissionMode, Tool};
    use std::fs;

    async fn output(context: &ToolContext, arguments: Value) -> ToolOutcome {
        JobOutputTool.execute(context, arguments).await
    }

    #[tokio::test]
    async fn captured_output_appears_and_job_list_reports_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, pid) = start_job(&context, "printf hello", None).await;

        let mut text = String::new();
        for _ in 0..100 {
            let outcome = output(&context, json!({"id": &id, "stream": "stdout"})).await;
            assert!(!outcome.is_error, "{}", outcome.text_content());
            text = outcome.text_content();
            if text == "hello" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(text, "hello");

        wait_for_status(&context, &id).await;
        let listed = crate::list::JobListTool
            .execute(&context, json!({}))
            .await
            .text_content();
        assert_eq!(listed, format!("{id}\t{pid}\t0"));
    }

    #[tokio::test]
    async fn a_requested_stream_is_returned_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, _pid) = start_job(&context, "printf out; printf err >&2", None).await;
        wait_for_status(&context, &id).await;

        let stdout = output(&context, json!({"id": &id, "stream": "stdout"})).await;
        assert_eq!(stdout.text_content(), "out");
        let stderr = output(&context, json!({"id": &id, "stream": "stderr"})).await;
        assert_eq!(stderr.text_content(), "err");
        let both = output(&context, json!({"id": &id})).await;
        assert_eq!(both.text_content(), "stdout:\nout\nstderr:\nerr");
    }

    #[tokio::test]
    async fn an_unknown_id_is_a_tool_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = output(&context, json!({"id": "deadbeef"})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("unknown job"));
    }

    #[tokio::test]
    async fn path_shaped_ids_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        for id in ["", "..", "../escape", "a/b", "a\\b", "a.b", "a\nb", "a b"] {
            let outcome = output(&context, json!({"id": id})).await;
            assert!(outcome.is_error, "{id:?}");
            assert!(
                outcome.text_content().contains("invalid job id"),
                "{id:?}: {}",
                outcome.text_content()
            );
        }
    }

    #[tokio::test]
    async fn unknown_stream_names_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = output(&context, json!({"id": "deadbeef", "stream": "both"})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));
    }

    #[tokio::test]
    async fn each_stream_is_capped_at_64_kib() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let jobs = job_directory(&context);
        fs::create_dir_all(&jobs).expect("mkdir");
        fs::write(jobs.join(format!("big{PID_SUFFIX}")), "4242").expect("write");
        fs::write(jobs.join("big.out"), "a".repeat(70_000)).expect("write");
        fs::write(jobs.join("big.err"), "b".repeat(70_000)).expect("write");

        let stdout = output(&context, json!({"id": "big", "stream": "stdout"})).await;
        assert_eq!(
            stdout.text_content().len(),
            usize::try_from(MAX_STREAM_BYTES).expect("cap")
        );
        let both = output(&context, json!({"id": "big"})).await;
        let text = both.text_content();
        assert_eq!(
            text.len(),
            usize::try_from(MAX_STREAM_BYTES).expect("cap") * 2 + "stdout:\n\nstderr:\n".len()
        );
    }

    #[tokio::test]
    async fn read_only_mode_can_read_job_output() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let jobs = job_directory(&context);
        fs::create_dir_all(&jobs).expect("mkdir");
        fs::write(jobs.join(format!("job{PID_SUFFIX}")), "4242").expect("write");
        fs::write(jobs.join("job.out"), "done").expect("write");
        fs::write(jobs.join("job.err"), "").expect("write");
        let outcome = output(&context, json!({"id": "job", "stream": "stdout"})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), "done");
    }
}
