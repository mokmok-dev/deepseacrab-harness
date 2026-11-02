//! The `ssh_run` tool.

use crate::config::SshHost;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Wall-clock limit applied when the model omits `timeout_ms`.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Largest accepted `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Bytes kept from each captured stream.
const MAX_STREAM_BYTES: usize = 64 * 1024;

/// Runs one command on a configured SSH host.
pub struct SshTool {
    hosts: Vec<SshHost>,
    ssh: PathBuf,
}

impl SshTool {
    /// Build the tool over the configured hosts.
    #[must_use]
    pub fn new(hosts: Vec<SshHost>, ssh: impl Into<PathBuf>) -> Self {
        Self {
            hosts,
            ssh: ssh.into(),
        }
    }

    fn host(&self, name: &str) -> Option<&SshHost> {
        self.hosts.iter().find(|host| host.name == name)
    }

    async fn run(
        &self,
        context: &ToolContext,
        host: &SshHost,
        command_line: &str,
        timeout_ms: u64,
    ) -> ToolOutcome {
        let mut command = Command::new(&self.ssh);
        command
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10");
        if let Some(port) = host.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(identity) = &host.identity {
            command.arg("-i").arg(identity);
        }
        command
            .arg(target(host))
            // ssh scans options across the whole argument list, so a command
            // that starts with `-` needs `--` to stay a command.
            .arg("--")
            .arg(command_line)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::select! {
            () = context.cancel.cancelled() => {
                return ToolOutcome::error("ssh_run cancelled");
            }
            result = tokio::time::timeout(Duration::from_millis(timeout_ms), command.output()) => {
                match result {
                    Err(_) => {
                        return ToolOutcome::error(format!(
                            "ssh_run timed out after {timeout_ms} ms"
                        ));
                    }
                    Ok(Err(error)) => {
                        return ToolOutcome::error(format!("failed to run ssh: {error}"));
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
}

#[derive(Deserialize)]
struct SshArguments {
    host: String,
    command: String,
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for SshTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ssh_run".to_string(),
            description: "Run a command on a configured remote host over SSH. The command runs \
                          directly, without a local shell. Returns the exit code, stdout, and \
                          stderr. Refused in read-only mode."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "host": {
                        "type": "string",
                        "description": "Name of a host configured in DSH_SSH_HOSTS."
                    },
                    "command": {
                        "type": "string",
                        "description": "Command line interpreted by the remote login shell."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Wall-clock limit in milliseconds, 1 to 600000. Defaults to 120000."
                    }
                },
                "required": ["host", "command"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: SshArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if !context.permission.allows_write() {
            return ToolOutcome::error(format!(
                "permission mode {} refuses ssh_run",
                context.permission.as_str()
            ));
        }
        let Some(host) = self.host(&arguments.host) else {
            return ToolOutcome::error(format!("unknown ssh host: {}", arguments.host));
        };
        if arguments.command.trim().is_empty() {
            return ToolOutcome::error("command must not be empty");
        }
        let timeout_ms = arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return ToolOutcome::error(format!(
                "timeout_ms must be in 1..={MAX_TIMEOUT_MS}, got {timeout_ms}"
            ));
        }
        self.run(context, host, &arguments.command, timeout_ms)
            .await
    }
}

fn target(host: &SshHost) -> String {
    host.user
        .as_ref()
        .map_or_else(|| host.host.clone(), |user| format!("{user}@{}", host.host))
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use dsh_core::PermissionMode;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    const RECORDER: &str = "#!/bin/sh\nscript_dir=$(dirname \"$0\")\n\
                            printf '%s\\n' \"$@\" >> \"$script_dir/argv.txt\"\n";

    static SCRIPTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        // A child forked by one test inherits another test's open script
        // handle, which makes that script fail to execute with ETXTBSY.
        SCRIPTS.lock().await
    }

    fn fake_ssh(directory: &Path, body: &str) -> PathBuf {
        let path = directory.join("ssh");
        std::fs::write(&path, format!("{RECORDER}{body}\n")).expect("write fake ssh");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    fn recorded(directory: &Path) -> Vec<String> {
        std::fs::read_to_string(directory.join("argv.txt"))
            .expect("argv recorded")
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    fn context(directory: &Path, permission: PermissionMode) -> ToolContext {
        ToolContext::new(
            directory.to_path_buf(),
            permission,
            CancellationToken::new(),
        )
    }

    fn host(name: &str, host: &str) -> SshHost {
        SshHost {
            name: name.to_string(),
            host: host.to_string(),
            user: None,
            port: None,
            identity: None,
        }
    }

    fn single(ssh: impl Into<PathBuf>) -> SshTool {
        SshTool::new(vec![host("web", "example.com")], ssh)
    }

    const fn arguments() -> &'static [&'static str] {
        &["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
    }

    #[test]
    fn the_schema_declares_ssh_run() {
        let schema = single("ssh").schema();
        assert_eq!(schema.name, "ssh_run");
        assert_ne!(schema.description, "");
        assert_eq!(schema.parameters["required"], json!(["host", "command"]));
        assert_eq!(schema.parameters["properties"]["host"]["type"], "string");
        assert_eq!(schema.parameters["properties"]["command"]["type"], "string");
        assert_eq!(
            schema.parameters["properties"]["timeout_ms"]["type"],
            "integer"
        );
    }

    #[tokio::test]
    async fn full_host_options_reach_ssh_in_order() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "printf 'out'; printf 'err' >&2");
        let identity = directory.path().join("id_ed25519");
        let tool = SshTool::new(
            vec![SshHost {
                name: "web".to_string(),
                host: "example.com".to_string(),
                user: Some("alice".to_string()),
                port: Some(2222),
                identity: Some(identity.clone()),
            }],
            &ssh,
        );

        let outcome = tool
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "echo hi; rm -rf /"}),
            )
            .await;

        assert!(!outcome.is_error, "{}", outcome.text_content());
        let mut expected: Vec<String> = arguments().iter().map(ToString::to_string).collect();
        expected.push("-p".to_string());
        expected.push("2222".to_string());
        expected.push("-i".to_string());
        expected.push(identity.to_str().expect("utf8").to_string());
        expected.push("alice@example.com".to_string());
        expected.push("--".to_string());
        expected.push("echo hi; rm -rf /".to_string());
        assert_eq!(recorded(directory.path()), expected);
        assert_eq!(
            outcome.text_content(),
            "exit_code: 0\nstdout:\nout\nstderr:\nerr"
        );
    }

    #[tokio::test]
    async fn hosts_without_overrides_use_the_bare_host() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "exit 0");
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "uptime"}),
            )
            .await;

        assert!(!outcome.is_error, "{}", outcome.text_content());
        let mut expected: Vec<String> = arguments().iter().map(ToString::to_string).collect();
        expected.push("example.com".to_string());
        expected.push("--".to_string());
        expected.push("uptime".to_string());
        assert_eq!(recorded(directory.path()), expected);
        assert_eq!(outcome.text_content(), "exit_code: 0\nstdout:\n\nstderr:\n");
    }

    #[tokio::test]
    async fn non_zero_exits_are_tool_errors() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "printf 'boom' >&2; exit 7");
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "false"}),
            )
            .await;

        assert!(outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "exit_code: 7\nstdout:\n\nstderr:\nboom"
        );
    }

    #[tokio::test]
    async fn signal_termination_reports_a_negative_exit_code() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "kill -9 $$");
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "true"}),
            )
            .await;

        assert!(outcome.is_error);
        assert!(
            outcome.text_content().starts_with("exit_code: -1\n"),
            "{}",
            outcome.text_content()
        );
    }

    #[tokio::test]
    async fn missing_executables_are_tool_errors() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let outcome = single(directory.path().join("missing-ssh"))
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "true"}),
            )
            .await;

        assert!(outcome.is_error);
        assert!(outcome.text_content().starts_with("failed to run ssh: "));
    }

    #[tokio::test]
    async fn read_only_mode_refuses_without_running_ssh() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "exit 0");
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::ReadOnly),
                json!({"host": "web", "command": "true"}),
            )
            .await;

        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
        assert!(!directory.path().join("argv.txt").exists());
    }

    #[tokio::test]
    async fn invalid_inputs_never_start_ssh() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "exit 0");
        let tool = single(&ssh);
        let workspace = context(directory.path(), PermissionMode::WorkspaceWrite);

        let cases = [
            (
                json!({"host": "missing", "command": "true"}),
                "unknown ssh host",
            ),
            (
                json!({"host": "web", "command": ""}),
                "command must not be empty",
            ),
            (
                json!({"host": "web", "command": "  \t"}),
                "command must not be empty",
            ),
            (
                json!({"host": "web", "command": "true", "timeout_ms": 0}),
                "timeout_ms must be in 1..=600000",
            ),
            (
                json!({"host": "web", "command": "true", "timeout_ms": 600_001}),
                "timeout_ms must be in 1..=600000",
            ),
            (json!({"host": "web"}), "invalid arguments"),
            (json!({"command": "true"}), "invalid arguments"),
            (json!({"host": 7, "command": "true"}), "invalid arguments"),
        ];
        for (arguments, expected) in cases {
            let outcome = tool.execute(&workspace, arguments.clone()).await;
            assert!(outcome.is_error, "{arguments} was accepted");
            assert!(
                outcome.text_content().contains(expected),
                "{arguments} reported {:?}",
                outcome.text_content()
            );
        }
        assert!(!directory.path().join("argv.txt").exists());
    }

    #[tokio::test]
    async fn omitted_timeouts_allow_slow_work() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "sleep 0.2");
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "sleep 0.2"}),
            )
            .await;

        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), "exit_code: 0\nstdout:\n\nstderr:\n");
    }

    #[tokio::test]
    async fn the_upper_timeout_bound_is_accepted() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "exit 0");
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "true", "timeout_ms": 600_000}),
            )
            .await;

        assert!(!outcome.is_error, "{}", outcome.text_content());
    }

    #[tokio::test]
    async fn slow_commands_are_killed_at_the_deadline() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "exec sleep 5");
        let started = Instant::now();
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "sleep 5", "timeout_ms": 100}),
            )
            .await;

        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), "ssh_run timed out after 100 ms");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn cancellation_kills_the_child() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(directory.path(), "exec sleep 5");
        let cancel = CancellationToken::new();
        let workspace = ToolContext::new(
            directory.path().to_path_buf(),
            PermissionMode::WorkspaceWrite,
            cancel.clone(),
        );
        let tool = single(&ssh);
        let started = Instant::now();
        let running = tokio::spawn(async move {
            tool.execute(
                &workspace,
                json!({"host": "web", "command": "sleep 5", "timeout_ms": 60_000}),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();

        let outcome = running.await.expect("join");
        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), "ssh_run cancelled");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn each_stream_is_capped_at_64_kib() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(
            directory.path(),
            "head -c 70000 /dev/zero | tr '\\0' 'a'; head -c 70000 /dev/zero | tr '\\0' 'b' >&2",
        );
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "spam"}),
            )
            .await;

        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(
            outcome.text_content(),
            format!(
                "exit_code: 0\nstdout:\n{}\nstderr:\n{}",
                "a".repeat(65_536),
                "b".repeat(65_536)
            )
        );
    }

    #[tokio::test]
    async fn truncation_stops_at_a_character_boundary() {
        let _guard = serial().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let ssh = fake_ssh(
            directory.path(),
            "i=0\nwhile [ \"$i\" -lt 33000 ]; do printf '\\303\\251'; i=$((i + 1)); done",
        );
        let outcome = single(&ssh)
            .execute(
                &context(directory.path(), PermissionMode::WorkspaceWrite),
                json!({"host": "web", "command": "accent"}),
            )
            .await;

        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(
            outcome.text_content(),
            format!(
                "exit_code: 0\nstdout:\n{}\nstderr:\n",
                "\u{e9}".repeat(32_768)
            )
        );
    }
}
