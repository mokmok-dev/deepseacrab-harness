//! The `job_start` tool.

use crate::files::{JOB_DIR, JobPaths};
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const TIMEOUT_STATUS: &str = "timeout";
const SIGNAL_STATUS: &str = "signal";

pub struct JobStartTool;

#[derive(Deserialize)]
struct StartArguments {
    command: String,
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for JobStartTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "job_start".to_string(),
            description: "Start a shell command in the session working directory with `bash -lc` \
                          and return immediately. Output is captured under `.dsh-jobs`; use \
                          job_output, job_list, and job_kill to inspect or stop the job. Refused \
                          in read-only mode."
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
                        "description": "Wall-clock limit in milliseconds. When it passes the job is killed and its status becomes `timeout`."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: StartArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if !context.permission.allows_write() {
            return ToolOutcome::error(format!(
                "permission mode {} refuses job_start",
                context.permission.as_str()
            ));
        }
        start(context, &arguments.command, arguments.timeout_ms)
    }
}

fn start(context: &ToolContext, command_line: &str, timeout_ms: Option<u64>) -> ToolOutcome {
    let directory = match context.writable_path(JOB_DIR) {
        Ok(directory) => directory,
        Err(error) => return ToolOutcome::error(error.to_string()),
    };
    if let Err(error) = fs::create_dir_all(&directory) {
        return ToolOutcome::error(format!("cannot create {}: {error}", directory.display()));
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let paths = JobPaths::new(&directory, &id);
    let stdout = match File::create(paths.stdout()) {
        Ok(file) => file,
        Err(error) => return ToolOutcome::error(format!("cannot create job output: {error}")),
    };
    let stderr = match File::create(paths.stderr()) {
        Ok(file) => file,
        Err(error) => return ToolOutcome::error(format!("cannot create job output: {error}")),
    };
    let mut child = match Command::new("bash")
        .arg("-lc")
        .arg(command_line)
        .current_dir(&context.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return ToolOutcome::error(format!("cannot start job: {error}")),
    };
    let pid = child.id();
    if let Err(error) = fs::write(paths.pid(), pid.to_string()) {
        let _ = child.kill();
        return ToolOutcome::error(format!("cannot record pid for job {id}: {error}"));
    }
    let status_path = paths.status();
    thread::spawn(move || {
        let label = match timeout_ms {
            Some(limit) => wait_with_timeout(&mut child, Duration::from_millis(limit)),
            None => child.wait().map(exit_label),
        };
        if let Ok(label) = label {
            record_status(&status_path, &label);
        }
    });
    ToolOutcome::text(format!("started {id} pid {pid}"))
}

/// Publish the final status by rename so a concurrent reader never sees the
/// file truncated to empty.
fn record_status(path: &Path, label: &str) {
    let temporary = path.with_extension("status.tmp");
    if fs::write(&temporary, label).is_ok() {
        let _ = fs::rename(&temporary, path);
    }
}

fn wait_with_timeout(child: &mut Child, limit: Duration) -> io::Result<String> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(exit_label(status));
        }
        if Instant::now().checked_duration_since(deadline).is_some() {
            child.kill()?;
            child.wait()?;
            return Ok(TIMEOUT_STATUS.to_string());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn exit_label(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| SIGNAL_STATUS.to_string(), |code| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{context, start_job, wait_for_status};
    use dsh_core::PermissionMode;
    use std::path::Path;

    #[tokio::test]
    async fn starting_a_job_reports_its_id_and_records_its_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, pid) = start_job(&context, "printf hello", None).await;
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let jobs = directory.path().join(JOB_DIR);
        assert_eq!(
            fs::read_to_string(jobs.join(format!("{id}.pid"))).expect("pid"),
            pid.to_string()
        );
        assert!(jobs.join(format!("{id}.out")).is_file());
        assert!(jobs.join(format!("{id}.err")).is_file());
    }

    #[tokio::test]
    async fn read_only_mode_refuses_job_start() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = JobStartTool
            .execute(&context, json!({"command": "printf hello"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
        assert!(!directory.path().join(JOB_DIR).exists());
    }

    #[tokio::test]
    async fn missing_commands_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = JobStartTool.execute(&context, json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));
    }

    #[tokio::test]
    async fn finished_jobs_record_their_exit_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, _pid) = start_job(&context, "exit 3", None).await;
        assert_eq!(wait_for_status(&context, &id).await, "3");
    }

    #[tokio::test]
    async fn jobs_that_finish_before_their_timeout_keep_their_exit_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, _pid) = start_job(&context, "printf hello", Some(30_000)).await;
        assert_eq!(wait_for_status(&context, &id).await, "0");
    }

    #[tokio::test]
    async fn timeouts_kill_the_job_and_record_a_timeout_status() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (id, pid) = start_job(&context, "sleep 5", Some(100)).await;
        assert_eq!(wait_for_status(&context, &id).await, TIMEOUT_STATUS);
        assert!(!Path::new("/proc").join(pid.to_string()).exists());
    }
}
