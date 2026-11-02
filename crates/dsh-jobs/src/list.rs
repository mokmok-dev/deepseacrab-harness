//! The `job_list` tool.

use crate::files::{JOB_DIR, JobPaths, PID_SUFFIX, read_pid, valid_job_id};
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const NO_JOBS: &str = "no jobs";
const RUNNING: &str = "running";
const UNKNOWN: &str = "unknown";

pub struct JobListTool;

#[async_trait]
impl Tool for JobListTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "job_list".to_string(),
            description: "List background jobs sorted by id, one `id<TAB>pid<TAB>status` line \
                          each. The status is the recorded exit code, `timeout`, `signal` when \
                          the job died from a signal, `running` while the process exists, or \
                          `unknown` otherwise."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, _arguments: Value) -> ToolOutcome {
        let directory = match context.readable_path(JOB_DIR) {
            Ok(directory) => directory,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        match list(&directory) {
            Ok(text) => ToolOutcome::text(text),
            Err(error) => ToolOutcome::error(error),
        }
    }
}

fn list(directory: &Path) -> Result<String, String> {
    if !directory.is_dir() {
        return Ok(NO_JOBS.to_string());
    }
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    let mut jobs: BTreeMap<String, u32> = BTreeMap::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(id) = name.strip_suffix(PID_SUFFIX) else {
            continue;
        };
        if !valid_job_id(id) {
            continue;
        }
        let Some(pid) = read_pid(&entry.path()) else {
            continue;
        };
        jobs.insert(id.to_string(), pid.get());
    }
    let lines: Vec<String> = jobs
        .iter()
        .map(|(id, pid)| format!("{id}\t{pid}\t{}", status_of(directory, id, *pid)))
        .collect();
    if lines.is_empty() {
        return Ok(NO_JOBS.to_string());
    }
    Ok(lines.join("\n"))
}

fn status_of(directory: &Path, id: &str, pid: u32) -> String {
    if let Ok(status) = fs::read_to_string(JobPaths::new(directory, id).status()) {
        return status;
    }
    if Path::new("/proc").join(pid.to_string()).exists() {
        RUNNING.to_string()
    } else {
        UNKNOWN.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kill::JobKillTool;
    use crate::test_util::{context, job_directory, start_job};
    use dsh_core::PermissionMode;

    async fn list_outcome(context: &ToolContext) -> String {
        let outcome = JobListTool.execute(context, json!({})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        outcome.text_content()
    }

    #[tokio::test]
    async fn a_missing_job_directory_has_no_jobs() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        assert_eq!(list_outcome(&context).await, NO_JOBS);
    }

    #[tokio::test]
    async fn job_directories_without_pid_files_have_no_jobs() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let jobs = job_directory(&context);
        fs::create_dir_all(&jobs).expect("mkdir");
        fs::write(jobs.join(".pid"), "1234").expect("write");
        fs::write(jobs.join("bad name.pid"), "1234").expect("write");
        fs::write(jobs.join("junk.pid"), "not-a-pid").expect("write");
        fs::write(jobs.join("notes.txt"), "irrelevant").expect("write");
        assert_eq!(list_outcome(&context).await, NO_JOBS);
    }

    #[tokio::test]
    async fn jobs_are_sorted_and_report_recorded_running_and_unknown_statuses() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let jobs = job_directory(&context);
        fs::create_dir_all(&jobs).expect("mkdir");
        fs::write(jobs.join("aaa.pid"), "4242").expect("write");
        fs::write(jobs.join("aaa.status"), "7").expect("write");
        fs::write(jobs.join("zzz.pid"), u32::MAX.to_string()).expect("write");
        let (live_id, live_process) = start_job(&context, "sleep 5", None).await;

        let text = list_outcome(&context).await;
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(lines.contains(&"aaa\t4242\t7"), "{text}");
        let unknown = format!("zzz\t{}\t{UNKNOWN}", u32::MAX);
        let running = format!("{live_id}\t{live_process}\t{RUNNING}");
        assert!(lines.iter().any(|line| *line == unknown), "{text}");
        assert!(lines.iter().any(|line| *line == running), "{text}");

        JobKillTool.execute(&context, json!({"id": live_id})).await;
    }

    #[tokio::test]
    async fn a_status_file_wins_over_a_live_process() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let jobs = job_directory(&context);
        fs::create_dir_all(&jobs).expect("mkdir");
        let pid = std::process::id();
        fs::write(jobs.join("done.pid"), pid.to_string()).expect("write");
        fs::write(jobs.join("done.status"), "0").expect("write");
        assert_eq!(list_outcome(&context).await, format!("done\t{pid}\t0"));
    }

    #[tokio::test]
    async fn read_only_mode_can_list_jobs() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let jobs = job_directory(&context);
        fs::create_dir_all(&jobs).expect("mkdir");
        fs::write(jobs.join("ghost.pid"), u32::MAX.to_string()).expect("write");
        assert!(list_outcome(&context).await.contains("ghost"));
    }
}
