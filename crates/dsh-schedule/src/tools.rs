//! The three model-facing schedule tools.

use crate::file::{MAX_INTERVAL_SECONDS, ScheduleFile};
use async_trait::async_trait;
use dsh_core::{Clock, SessionId, SystemClock, Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};

const SCHEDULE_FILE: &str = "schedules.json";

const NO_SCHEDULES: &str = "no schedules";

fn schedule_file(context: &ToolContext) -> ScheduleFile {
    ScheduleFile::new(context.state_dir.join(SCHEDULE_FILE))
}

fn now_millis(context: &ToolContext) -> i64 {
    context
        .session()
        .map_or_else(|| SystemClock.now_millis(), |session| session.now_millis())
}

/// Whether `id` may name a stored schedule.
///
/// The schedule file is keyed by id, so the tool accepts only the token shape
/// [`ScheduleFile::create`](crate::ScheduleFile::create) mints.
fn valid_schedule_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Create a durable recurring follow-up.
pub struct ScheduleCreateTool;

#[derive(Deserialize)]
struct CreateArguments {
    task: String,
    interval_seconds: u64,
    #[serde(default)]
    session_id: Option<String>,
}

#[async_trait]
impl Tool for ScheduleCreateTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "schedule_create".to_string(),
            description: "Create a durable recurring follow-up. The task runs in the recorded \
                          session every `interval_seconds` seconds; omit `session_id` to keep \
                          using the current session."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Prompt replayed on every due run"
                    },
                    "interval_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_INTERVAL_SECONDS
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session that receives each run"
                    }
                },
                "required": ["task", "interval_seconds"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: CreateArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        let session_id = match arguments.session_id {
            Some(raw) => match SessionId::parse(raw) {
                Ok(id) => Some(id),
                Err(error) => {
                    return ToolOutcome::error(format!("invalid session_id: {error}"));
                }
            },
            None => context
                .session()
                .map(|session| session.session_id().clone()),
        };
        let file = schedule_file(context);
        match file.create(
            &arguments.task,
            arguments.interval_seconds,
            session_id,
            context.cwd.display().to_string(),
            now_millis(context),
        ) {
            Ok(schedule) => ToolOutcome::text(format!(
                "created {} next_run_at {}",
                schedule.id, schedule.next_run_at
            )),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

/// List durable recurring follow-ups.
pub struct ScheduleListTool;

#[async_trait]
impl Tool for ScheduleListTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "schedule_list".to_string(),
            description: "List schedules, one `id<TAB>next_run_at<TAB>enabled<TAB>task` line \
                          each."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, _arguments: Value) -> ToolOutcome {
        let schedules = match schedule_file(context).list() {
            Ok(schedules) => schedules,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        if schedules.is_empty() {
            return ToolOutcome::text(NO_SCHEDULES);
        }
        let lines: Vec<String> = schedules
            .iter()
            .map(|schedule| {
                format!(
                    "{}\t{}\t{}\t{}",
                    schedule.id, schedule.next_run_at, schedule.enabled, schedule.task
                )
            })
            .collect();
        ToolOutcome::text(lines.join("\n"))
    }
}

/// Cancel a durable recurring follow-up.
pub struct ScheduleCancelTool;

#[derive(Deserialize)]
struct CancelArguments {
    id: String,
}

#[async_trait]
impl Tool for ScheduleCancelTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "schedule_cancel".to_string(),
            description: "Cancel one schedule by id.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Schedule id returned by schedule_create or schedule_list"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: CancelArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if !valid_schedule_id(&arguments.id) {
            return ToolOutcome::error(format!("invalid schedule id: {}", arguments.id));
        }
        match schedule_file(context).cancel(&arguments.id) {
            Ok(true) => ToolOutcome::text(format!("cancelled {}", arguments.id)),
            Ok(false) => ToolOutcome::error(format!("schedule {} not found", arguments.id)),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::{EventRecord, PermissionMode, SessionEvent, StoreError, ToolSession};
    use std::fs;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct TestSession {
        id: SessionId,
        now: i64,
    }

    impl ToolSession for TestSession {
        fn session_id(&self) -> &SessionId {
            &self.id
        }

        fn append(&self, _event: SessionEvent) -> Result<EventRecord, StoreError> {
            Err(StoreError::Unsupported("append".to_string()))
        }

        fn now_millis(&self) -> i64 {
            self.now
        }
    }

    fn fixture() -> (tempfile::TempDir, ToolContext) {
        let directory = tempfile::tempdir().expect("tempdir");
        let cwd = directory.path().join("cwd");
        fs::create_dir_all(&cwd).expect("cwd");
        let state = directory.path().join("state");
        let context = ToolContext::new(
            cwd,
            PermissionMode::WorkspaceWrite,
            CancellationToken::new(),
        )
        .with_state_dir(state);
        (directory, context)
    }

    fn schedules(context: &ToolContext) -> Vec<crate::Schedule> {
        schedule_file(context).load().expect("load")
    }

    async fn create(context: &ToolContext, arguments: Value) -> ToolOutcome {
        ScheduleCreateTool.execute(context, arguments).await
    }

    #[tokio::test]
    async fn create_uses_the_current_session_and_reports_the_due_time() {
        let (_directory, context) = fixture();
        let session = Arc::new(TestSession {
            id: SessionId::parse("session-current").expect("id"),
            now: 7_000,
        });
        let context = context.with_session(session.clone());
        let outcome = create(&context, json!({"task": "ping", "interval_seconds": 60})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        let stored = schedules(&context);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].session_id.as_ref(), Some(session.session_id()));
        assert_eq!(stored[0].task, "ping");
        assert_eq!(stored[0].next_run_at, 67_000);
        assert_eq!(stored[0].cwd, context.cwd.display().to_string());
        assert!(stored[0].enabled);
        assert_eq!(
            outcome.text_content(),
            format!("created {} next_run_at 67000", stored[0].id)
        );
    }

    #[tokio::test]
    async fn create_without_a_session_records_none() {
        let (_directory, context) = fixture();
        let outcome = create(&context, json!({"task": "ping", "interval_seconds": 60})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(schedules(&context)[0].session_id, None);
    }

    #[tokio::test]
    async fn create_records_an_explicit_session_id() {
        let (_directory, context) = fixture();
        let outcome = create(
            &context,
            json!({"task": "ping", "interval_seconds": 60, "session_id": "session-named"}),
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(
            schedules(&context)[0].session_id,
            Some(SessionId::parse("session-named").expect("id"))
        );
    }

    #[tokio::test]
    async fn create_writes_under_the_state_directory() {
        let (_directory, context) = fixture();
        create(&context, json!({"task": "ping", "interval_seconds": 60})).await;
        assert!(context.state_dir.join(SCHEDULE_FILE).exists());
        assert!(!context.cwd.join(SCHEDULE_FILE).exists());
    }

    #[tokio::test]
    async fn create_rejects_bad_input() {
        let (_directory, context) = fixture();
        for arguments in [
            json!({"task": "", "interval_seconds": 60}),
            json!({"task": "   ", "interval_seconds": 60}),
            json!({"task": "ping", "interval_seconds": 0}),
            json!({"task": "ping", "interval_seconds": MAX_INTERVAL_SECONDS + 1}),
            json!({"task": "ping", "interval_seconds": -1}),
            json!({"task": "ping", "interval_seconds": 60, "session_id": ""}),
            json!({"task": "ping"}),
            json!({"interval_seconds": 60}),
        ] {
            let outcome = create(&context, arguments.clone()).await;
            assert!(outcome.is_error, "{arguments} was accepted");
        }
        assert_eq!(schedules(&context), Vec::new());
    }

    #[tokio::test]
    async fn list_renders_one_line_per_schedule() {
        let (_directory, context) = fixture();
        assert_eq!(
            ScheduleListTool
                .execute(&context, json!({}))
                .await
                .text_content(),
            NO_SCHEDULES
        );
        let first = create(&context, json!({"task": "first", "interval_seconds": 60})).await;
        let second = create(&context, json!({"task": "second", "interval_seconds": 120})).await;
        assert!(!first.is_error);
        assert!(!second.is_error);
        let stored = schedules(&context);
        let expected: Vec<String> = stored
            .iter()
            .map(|schedule| {
                format!(
                    "{}\t{}\ttrue\t{}",
                    schedule.id, schedule.next_run_at, schedule.task
                )
            })
            .collect();
        assert_eq!(
            ScheduleListTool
                .execute(&context, json!({}))
                .await
                .text_content(),
            expected.join("\n")
        );
    }

    #[tokio::test]
    async fn cancel_removes_the_named_schedule() {
        let (_directory, context) = fixture();
        create(&context, json!({"task": "first", "interval_seconds": 60})).await;
        let stored = schedules(&context);
        let outcome = ScheduleCancelTool
            .execute(&context, json!({"id": stored[0].id}))
            .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(
            outcome.text_content(),
            format!("cancelled {}", stored[0].id)
        );
        assert_eq!(schedules(&context), Vec::new());
    }

    #[tokio::test]
    async fn cancel_rejects_missing_and_malformed_ids() {
        let (_directory, context) = fixture();
        let missing = ScheduleCancelTool
            .execute(&context, json!({"id": "schedule-missing"}))
            .await;
        assert!(missing.is_error);
        assert!(missing.text_content().contains("not found"));
        for id in ["", "..", "a/b", "a b", "a\nb"] {
            let outcome = ScheduleCancelTool
                .execute(&context, json!({"id": id}))
                .await;
            assert!(outcome.is_error, "{id:?} was accepted");
            assert!(outcome.text_content().contains("invalid schedule id"));
        }
        let malformed = ScheduleCancelTool.execute(&context, json!({})).await;
        assert!(malformed.is_error);
    }

    #[tokio::test]
    async fn malformed_arguments_are_tool_errors() {
        let (_directory, context) = fixture();
        let arguments = Value::String("nope".to_string());
        assert!(
            ScheduleCreateTool
                .execute(&context, arguments)
                .await
                .is_error
        );
        assert!(ScheduleListTool.run(&context, "[]").await.is_error);
    }

    #[tokio::test]
    async fn generated_schedule_ids_pass_the_cancel_guard() {
        let (_directory, context) = fixture();
        create(&context, json!({"task": "ping", "interval_seconds": 1})).await;
        let stored = schedules(&context);
        assert!(valid_schedule_id(&stored[0].id));
        assert!(!valid_schedule_id(""));
        assert!(!valid_schedule_id("a/b"));
        assert!(valid_schedule_id("schedule-1_a"));
    }
}
