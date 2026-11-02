//! Shared fixtures for job tool tests.

use crate::files::{JOB_DIR, JobPaths};
use crate::start::JobStartTool;
use dsh_core::{PermissionMode, Tool, ToolContext};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn context(directory: &std::path::Path, permission: PermissionMode) -> ToolContext {
    ToolContext::new(
        directory.to_path_buf(),
        permission,
        CancellationToken::new(),
    )
}

pub fn job_directory(context: &ToolContext) -> PathBuf {
    context.cwd.join(JOB_DIR)
}

pub async fn start_job(
    context: &ToolContext,
    command: &str,
    timeout_ms: Option<u64>,
) -> (String, u32) {
    let arguments = timeout_ms.map_or_else(
        || json!({"command": command}),
        |timeout_ms| json!({"command": command, "timeout_ms": timeout_ms}),
    );
    let outcome = JobStartTool.execute(context, arguments).await;
    assert!(!outcome.is_error, "{}", outcome.text_content());
    let text = outcome.text_content();
    let parts: Vec<&str> = text.split_whitespace().collect();
    assert_eq!(parts.len(), 4, "{text}");
    assert_eq!(parts[0], "started", "{text}");
    assert_eq!(parts[2], "pid", "{text}");
    (parts[1].to_string(), parts[3].parse().expect("pid"))
}

pub async fn wait_for_status(context: &ToolContext, id: &str) -> String {
    let path = JobPaths::new(&job_directory(context), id).status();
    for _ in 0..100 {
        if let Ok(status) = std::fs::read_to_string(&path) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("status file for {id} did not appear");
}
