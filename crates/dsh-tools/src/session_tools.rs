//! Session-owned tools: todos, plans, goals, and session search.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{
    GoalPhase, SessionEvent, TodoItem, TodoStatus, Tool, ToolContext, ToolOutcome, ToolSchema,
    ToolSession,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

fn session(context: &ToolContext) -> Result<&Arc<dyn ToolSession>, ToolOutcome> {
    context
        .session()
        .ok_or_else(|| ToolOutcome::error("this tool requires a durable session"))
}

/// Replace the session todo list.
pub struct TodoWriteTool;

#[derive(Deserialize)]
struct TodoArgs {
    todos: Vec<TodoItem>,
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "todo_write".to_string(),
            description: "Replace the session todo list and render the checklist.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "text": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            },
                            "required": ["id", "text", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: TodoArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args
            .todos
            .iter()
            .any(|item| item.id.trim().is_empty() || item.text.trim().is_empty())
        {
            return ToolOutcome::error("todo ids and text must not be empty");
        }
        let session = match session(context) {
            Ok(session) => session,
            Err(error) => return error,
        };
        if let Err(error) = session.append(SessionEvent::TodoWritten {
            items: args.todos.clone(),
        }) {
            return ToolOutcome::error(error.to_string());
        }
        ToolOutcome::text(render_todos(&args.todos))
    }
}

fn render_todos(items: &[TodoItem]) -> String {
    let mut output = String::new();
    for item in items {
        let mark = match item.status {
            TodoStatus::Pending => " ",
            TodoStatus::InProgress => "~",
            TodoStatus::Completed => "x",
        };
        output.push_str(&format!("[{mark}] {} {}\n", item.id, item.text));
    }
    output.push_str(&format!("{} todos", items.len()));
    output
}

/// Set the session plan.
pub struct PlanTool;

#[derive(Deserialize)]
struct PlanArgs {
    steps: Vec<String>,
    #[serde(default)]
    approved: bool,
}

#[async_trait]
impl Tool for PlanTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "update_plan".to_string(),
            description: "Record or replace the ordered session plan.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "steps": {"type": "array", "items": {"type": "string"}},
                    "approved": {"type": "boolean"}
                },
                "required": ["steps"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: PlanArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args.steps.is_empty() || args.steps.iter().any(|step| step.trim().is_empty()) {
            return ToolOutcome::error("plan steps must not be empty");
        }
        let session = match session(context) {
            Ok(session) => session,
            Err(error) => return error,
        };
        if let Err(error) = session.append(SessionEvent::PlanSet {
            steps: args.steps.clone(),
            approved: args.approved,
        }) {
            return ToolOutcome::error(error.to_string());
        }
        let mut output = String::new();
        for (index, step) in args.steps.iter().enumerate() {
            output.push_str(&format!("{}. {step}\n", index + 1));
        }
        output.push_str(if args.approved {
            "plan approved"
        } else {
            "plan proposed"
        });
        ToolOutcome::text(output)
    }
}

/// Set the session goal.
pub struct GoalTool;

#[derive(Deserialize)]
struct GoalArgs {
    objective: String,
    #[serde(default)]
    phase: Option<GoalPhase>,
}

#[async_trait]
impl Tool for GoalTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "create_goal".to_string(),
            description: "Attach or update the durable completion objective for this session."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "objective": {"type": "string"},
                    "phase": {"type": "string", "enum": ["active", "paused", "blocked", "complete"]}
                },
                "required": ["objective"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: GoalArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args.objective.trim().is_empty() {
            return ToolOutcome::error("goal objective must not be empty");
        }
        let phase = args.phase.unwrap_or(GoalPhase::Active);
        let session = match session(context) {
            Ok(session) => session,
            Err(error) => return error,
        };
        if let Err(error) = session.append(SessionEvent::GoalSet {
            objective: args.objective.clone(),
            phase,
        }) {
            return ToolOutcome::error(error.to_string());
        }
        ToolOutcome::text(format!("goal {phase:?}: {}", args.objective))
    }
}

/// Search the current session's durable event log.
pub struct SessionSearchTool;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    20
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "session_search".to_string(),
            description: "Search durable events in the current session.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: SearchArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args.query.trim().is_empty() {
            return ToolOutcome::error("search query must not be empty");
        }
        if args.limit == 0 || args.limit > 100 {
            return ToolOutcome::error("search limit must be between 1 and 100");
        }
        let session = match session(context) {
            Ok(session) => session,
            Err(error) => return error,
        };
        let hits = match session.search(&args.query, args.limit) {
            Ok(hits) => hits,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        if hits.is_empty() {
            return ToolOutcome::text("no matches");
        }
        let mut output = String::new();
        for hit in hits {
            output.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                hit.session_id, hit.seq, hit.kind, hit.snippet
            ));
        }
        ToolOutcome::text(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::session_context;
    use dsh_core::{EventSeq, PermissionMode, SessionId, SessionSearchHit};

    fn context(permission: PermissionMode) -> (ToolContext, Arc<crate::test_util::MockSession>) {
        let directory = std::env::temp_dir();
        session_context(&directory, permission)
    }

    #[tokio::test]
    async fn todo_write_records_and_renders_the_list() {
        let (context, session) = context(PermissionMode::WorkspaceWrite);
        let outcome = TodoWriteTool
            .execute(
                &context,
                json!({"todos": [
                    {"id": "1", "text": "read", "status": "completed"},
                    {"id": "2", "text": "write", "status": "in_progress"},
                    {"id": "3", "text": "test", "status": "pending"}
                ]}),
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "[x] 1 read\n[~] 2 write\n[ ] 3 test\n3 todos"
        );
        let events = session.events();
        assert!(matches!(
            &events[0],
            SessionEvent::TodoWritten { items } if items.len() == 3
        ));
    }

    #[tokio::test]
    async fn todo_write_rejects_empty_items() {
        let (context, _session) = context(PermissionMode::WorkspaceWrite);
        let outcome = TodoWriteTool
            .execute(
                &context,
                json!({"todos": [{"id": "", "text": "x", "status": "pending"}]}),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("must not be empty"));
    }

    #[tokio::test]
    async fn plan_records_steps_and_approval() {
        let (context, session) = context(PermissionMode::WorkspaceWrite);
        let outcome = PlanTool
            .execute(
                &context,
                json!({"steps": ["inspect", "change"], "approved": true}),
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "1. inspect\n2. change\nplan approved"
        );
        assert!(matches!(
            &session.events()[0],
            SessionEvent::PlanSet { steps, approved: true } if steps.len() == 2
        ));
    }

    #[tokio::test]
    async fn plan_rejects_empty_steps() {
        let (context, _session) = context(PermissionMode::WorkspaceWrite);
        let outcome = PlanTool.execute(&context, json!({"steps": []})).await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn goal_defaults_to_active() {
        let (context, session) = context(PermissionMode::WorkspaceWrite);
        let outcome = GoalTool
            .execute(&context, json!({"objective": "ship the rewrite"}))
            .await;
        assert!(!outcome.is_error);
        assert!(outcome.text_content().contains("ship the rewrite"));
        assert!(matches!(
            &session.events()[0],
            SessionEvent::GoalSet {
                phase: GoalPhase::Active,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn goal_rejects_empty_objectives() {
        let (context, _session) = context(PermissionMode::WorkspaceWrite);
        let outcome = GoalTool.execute(&context, json!({"objective": " "})).await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn session_search_returns_hits() {
        let (context, session) = context(PermissionMode::WorkspaceWrite);
        session.set_hits(vec![SessionSearchHit {
            session_id: SessionId::generate(),
            seq: EventSeq::new(3),
            kind: "user_message".to_string(),
            snippet: "the answer".to_string(),
        }]);
        assert_eq!(default_limit(), 20);
        let outcome = SessionSearchTool
            .execute(&context, json!({"query": "answer"}))
            .await;
        assert!(!outcome.is_error);
        assert!(outcome.text_content().contains("user_message"));
        assert!(outcome.text_content().contains("the answer"));
        let full = SessionSearchTool
            .execute(&context, json!({"query": "answer", "limit": 100}))
            .await;
        assert!(!full.is_error);
    }

    #[tokio::test]
    async fn session_tools_require_a_session() {
        let context =
            crate::test_util::context(&std::env::temp_dir(), PermissionMode::WorkspaceWrite);
        for outcome in [
            TodoWriteTool.execute(&context, json!({"todos": []})).await,
            PlanTool.execute(&context, json!({"steps": ["x"]})).await,
            GoalTool.execute(&context, json!({"objective": "x"})).await,
            SessionSearchTool
                .execute(&context, json!({"query": "x"}))
                .await,
        ] {
            assert!(outcome.is_error);
            assert!(outcome.text_content().contains("durable session"));
        }
    }

    #[tokio::test]
    async fn session_search_rejects_bad_bounds() {
        let (context, _session) = context(PermissionMode::WorkspaceWrite);
        assert!(
            SessionSearchTool
                .execute(&context, json!({"query": ""}))
                .await
                .is_error
        );
        assert!(
            SessionSearchTool
                .execute(&context, json!({"query": "x", "limit": 0}))
                .await
                .is_error
        );
        assert!(
            SessionSearchTool
                .execute(&context, json!({"query": "x", "limit": 101}))
                .await
                .is_error
        );
    }
}
