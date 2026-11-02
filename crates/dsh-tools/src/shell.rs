//! The `bash` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{SandboxCommand, Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_STREAM_BYTES: usize = 64 * 1024;

pub struct BashTool;

#[derive(Deserialize)]
struct BashArguments {
    command: String,
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bash".to_string(),
            description: "Run a shell command in the session working directory with `bash -lc`. \
                          Returns the exit code, stdout, and stderr. Refused in read-only mode."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command line passed to `bash -lc`."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Wall-clock limit in milliseconds. Defaults to 120000."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: BashArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        if !context.permission.allows_write() {
            return ToolOutcome::error(format!(
                "permission mode {} refuses bash",
                context.permission.as_str()
            ));
        }
        run(context, &arguments.command, arguments.timeout_ms).await
    }
}

async fn run(context: &ToolContext, command_line: &str, timeout_ms: Option<u64>) -> ToolOutcome {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let spec = context.sandbox.as_ref().map_or_else(
        || SandboxCommand::shell(command_line),
        |sandbox| sandbox.wrap(command_line, &context.cwd, context.permission),
    );
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&context.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::select! {
        () = context.cancel.cancelled() => return ToolOutcome::error("bash cancelled"),
        result = tokio::time::timeout(Duration::from_millis(timeout_ms), command.output()) => {
            match result {
                Err(_) => {
                    return ToolOutcome::error(format!("bash timed out after {timeout_ms} ms"));
                }
                Ok(Err(error)) => {
                    return ToolOutcome::error(format!("failed to run bash: {error}"));
                }
                Ok(Ok(output)) => output,
            }
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format_output(output.status.code().unwrap_or(-1), &stdout, &stderr);
    if output.status.success() {
        ToolOutcome::text(text)
    } else {
        ToolOutcome::error(text)
    }
}

fn format_output(exit_code: i32, stdout: &str, stderr: &str) -> String {
    format!(
        "exit_code: {exit_code}\nstdout:\n{}\nstderr:\n{}",
        truncate_stream(stdout),
        truncate_stream(stderr)
    )
}

fn truncate_stream(stream: &str) -> &str {
    if let Some(prefix) = stream.get(..MAX_STREAM_BYTES) {
        return prefix;
    }
    let end = floor_char_boundary(stream, MAX_STREAM_BYTES);
    &stream[..end]
}

fn floor_char_boundary(stream: &str, mut index: usize) -> usize {
    while !stream.is_char_boundary(index) {
        if index == 0 {
            break;
        }
        index = index.saturating_sub(1);
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn successful_commands_report_both_streams() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = BashTool
            .execute(&context, json!({"command": "printf out; printf err >&2"}))
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "exit_code: 0\nstdout:\nout\nstderr:\nerr"
        );
    }

    #[tokio::test]
    async fn non_zero_exits_are_tool_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = BashTool
            .execute(&context, json!({"command": "printf oops >&2; exit 7"}))
            .await;
        assert!(outcome.is_error);
        let text = outcome.text_content();
        assert!(text.starts_with("exit_code: 7\nstdout:\n"));
        assert!(text.contains("\nstderr:\noops"));
    }

    #[tokio::test]
    async fn commands_run_in_the_session_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let canonical = directory.path().canonicalize().expect("canonical");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = BashTool
            .execute(&context, json!({"command": "pwd -P"}))
            .await;
        assert!(!outcome.is_error);
        assert!(
            outcome
                .text_content()
                .contains(canonical.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn read_only_mode_refuses_bash() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = BashTool
            .execute(&context, json!({"command": "echo hi"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
    }

    #[tokio::test]
    async fn timeouts_kill_the_child() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let started = Instant::now();
        let outcome = BashTool
            .execute(
                &context,
                json!({"command": "exec sleep 5", "timeout_ms": 100}),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("timed out after 100 ms"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn cancellation_kills_the_child() {
        let directory = tempfile::tempdir().expect("tempdir");
        let cancel = CancellationToken::new();
        let context = ToolContext::new(
            directory.path().to_path_buf(),
            PermissionMode::WorkspaceWrite,
            cancel.clone(),
        );
        let started = Instant::now();
        let running = tokio::spawn(async move {
            BashTool
                .execute(
                    &context,
                    json!({"command": "exec sleep 5", "timeout_ms": 60_000}),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let outcome = running.await.expect("join");
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn missing_commands_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = BashTool.execute(&context, json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));
    }

    #[test]
    fn default_timeout_is_two_minutes() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 120_000);
    }

    #[test]
    fn streams_are_truncated_to_64_kib() {
        let long = "a".repeat(MAX_STREAM_BYTES + 10);
        assert_eq!(truncate_stream(&long).len(), MAX_STREAM_BYTES);
        assert_eq!(MAX_STREAM_BYTES, 65_536);
        assert_eq!(truncate_stream("short"), "short");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_termination_reports_negative_exit_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = BashTool
            .execute(&context, json!({"command": "kill -9 $$"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().starts_with("exit_code: -1\n"));
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let mut text = "a".repeat(MAX_STREAM_BYTES - 1);
        text.push('é');
        let truncated = truncate_stream(&text);
        assert_eq!(truncated.len(), MAX_STREAM_BYTES - 1);
        assert!(truncated.chars().all(|character| character == 'a'));
    }

    #[test]
    fn short_streams_are_unchanged() {
        assert_eq!(truncate_stream("short"), "short");
        assert_eq!(
            format_output(0, "a", "b"),
            "exit_code: 0\nstdout:\na\nstderr:\nb"
        );
    }

    struct RecordingSandbox;

    impl dsh_core::Sandbox for RecordingSandbox {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn wrap(
            &self,
            command: &str,
            _cwd: &std::path::Path,
            _permission: PermissionMode,
        ) -> dsh_core::SandboxCommand {
            dsh_core::SandboxCommand {
                program: "bash".to_string(),
                args: vec!["-lc".to_string(), format!("echo sandboxed; {command}")],
            }
        }
    }

    #[tokio::test]
    async fn sandbox_commands_are_used() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite)
            .with_sandbox(std::sync::Arc::new(RecordingSandbox));
        let outcome = BashTool.execute(&context, json!({"command": "true"})).await;
        assert!(!outcome.is_error);
        assert!(outcome.text_content().contains("sandboxed"));
    }
}
