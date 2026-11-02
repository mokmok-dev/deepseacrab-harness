//! The seven model-facing team tools.

use crate::model::{MemberStatus, TaskStatus, TeamMember};
use crate::runtime::TeamRuntime;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// Ceiling on one `wait_agent` call.
pub const WAIT_CEILING: Duration = Duration::from_secs(300);
const WAIT_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamToolKind {
    SpawnTeammate,
    ListAgents,
    SendMessage,
    WaitAgent,
    TaskCreate,
    TaskList,
    TaskUpdate,
}

impl TeamToolKind {
    pub const ALL: [Self; 7] = [
        Self::SpawnTeammate,
        Self::ListAgents,
        Self::SendMessage,
        Self::WaitAgent,
        Self::TaskCreate,
        Self::TaskList,
        Self::TaskUpdate,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::SpawnTeammate => "spawn_teammate",
            Self::ListAgents => "list_agents",
            Self::SendMessage => "send_message",
            Self::WaitAgent => "wait_agent",
            Self::TaskCreate => "team_task_create",
            Self::TaskList => "team_task_list",
            Self::TaskUpdate => "team_task_update",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::SpawnTeammate => {
                "Spawn a teammate that runs a task in a fresh child session. Returns the member \
                 id and the session id."
            }
            Self::ListAgents => {
                "List teammates, one `id<TAB>name<TAB>status<TAB>session` line each."
            }
            Self::SendMessage => "Send another turn to an idle teammate.",
            Self::WaitAgent => {
                "Wait up to 300 seconds for a teammate to leave the running state, then return \
                 its status and last text."
            }
            Self::TaskCreate => "Add a task to the shared team task list.",
            Self::TaskList => "List team tasks, one `id<TAB>status<TAB>title` line each.",
            Self::TaskUpdate => "Move one team task to another status.",
        }
    }

    fn parameters(self) -> Value {
        match self {
            Self::SpawnTeammate => json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Human-readable teammate name"
                    },
                    "task": {
                        "type": "string",
                        "description": "Task to run in the teammate's child session"
                    }
                },
                "required": ["name", "task"]
            }),
            Self::ListAgents | Self::TaskList => json!({
                "type": "object",
                "properties": {}
            }),
            Self::SendMessage => json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "Member id returned by spawn_teammate"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message appended as the next user turn"
                    }
                },
                "required": ["agent_id", "message"]
            }),
            Self::WaitAgent => json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "Member id returned by spawn_teammate"
                    }
                },
                "required": ["agent_id"]
            }),
            Self::TaskCreate => json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short task summary"
                    },
                    "description": {
                        "type": "string",
                        "description": "Full task body"
                    }
                },
                "required": ["title", "description"]
            }),
            Self::TaskUpdate => json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Task id returned by team_task_create or team_task_list"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "completed"]
                    }
                },
                "required": ["id", "status"]
            }),
        }
    }
}

pub struct TeamTool {
    runtime: Arc<TeamRuntime>,
    kind: TeamToolKind,
}

impl TeamTool {
    pub(crate) fn all(runtime: &Arc<TeamRuntime>) -> Vec<Arc<dyn Tool>> {
        TeamToolKind::ALL
            .into_iter()
            .map(|kind| {
                Arc::new(Self {
                    runtime: Arc::clone(runtime),
                    kind,
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    fn spawn_teammate(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: SpawnArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        let Some(session) = context.session() else {
            return ToolOutcome::error("this tool requires a durable session");
        };
        let name = arguments.name.trim();
        if name.is_empty() {
            return ToolOutcome::error("teammate name must not be empty");
        }
        if arguments.task.trim().is_empty() {
            return ToolOutcome::error("teammate task must not be empty");
        }
        let member = match self.runtime.add_member(name) {
            Ok(member) => member,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let runtime = Arc::clone(&self.runtime);
        let member_id = member.id.clone();
        let session_id = member.session_id.clone();
        let task = arguments.task;
        let parent = session.session_id().clone();
        let cwd = context.cwd.clone();
        let cancel = context.cancel.clone();
        tokio::spawn(async move {
            let result = match runtime.child_agent(cwd, Some(parent)) {
                Ok(agent) => agent.run(&session_id, &task, cancel, None).await,
                Err(error) => Err(error),
            };
            runtime.settle(&member_id, result);
        });
        ToolOutcome::text(format!(
            "spawned {} session {}",
            member.id, member.session_id
        ))
    }

    fn list_agents(&self) -> ToolOutcome {
        let members = match self.runtime.list_members() {
            Ok(members) => members,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let lines: Vec<String> = members
            .iter()
            .map(|member| {
                format!(
                    "{}\t{}\t{}\t{}",
                    member.id,
                    member.name,
                    member.status.as_str(),
                    member.session_id
                )
            })
            .collect();
        ToolOutcome::text(lines.join("\n"))
    }

    fn send_message(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: SendArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        let member = match self
            .runtime
            .message_member(&arguments.agent_id, &arguments.message)
        {
            Ok(member) => member,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let runtime = Arc::clone(&self.runtime);
        let member_id = member.id.clone();
        let session_id = member.session_id.clone();
        let message = arguments.message;
        let cwd = context.cwd.clone();
        let cancel = context.cancel.clone();
        tokio::spawn(async move {
            let result = match runtime.child_agent(cwd, None) {
                Ok(agent) => agent.run(&session_id, &message, cancel, None).await,
                Err(error) => Err(error),
            };
            runtime.settle(&member_id, result);
        });
        ToolOutcome::text(format!("sent {}", member.id))
    }

    async fn wait_agent(&self, arguments: Value) -> ToolOutcome {
        let arguments: WaitArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        let deadline = Instant::now() + WAIT_CEILING;
        loop {
            let member = match self.runtime.member(&arguments.agent_id) {
                Ok(member) => member,
                Err(error) => return ToolOutcome::error(error.to_string()),
            };
            if member.status != MemberStatus::Running {
                return member_report(&member);
            }
            if past_deadline(Instant::now(), deadline) {
                return member_report(&member);
            }
            sleep(WAIT_POLL).await;
        }
    }

    fn create_task(&self, arguments: Value) -> ToolOutcome {
        let arguments: CreateTaskArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        let title = arguments.title.trim();
        if title.is_empty() {
            return ToolOutcome::error("team task title must not be empty");
        }
        match self.runtime.create_task(title, &arguments.description) {
            Ok(task) => ToolOutcome::text(format!("created {}", task.id)),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }

    fn list_tasks(&self) -> ToolOutcome {
        let tasks = match self.runtime.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let lines: Vec<String> = tasks
            .iter()
            .map(|task| format!("{}\t{}\t{}", task.id, task.status.as_str(), task.title))
            .collect();
        ToolOutcome::text(lines.join("\n"))
    }

    fn update_task(&self, arguments: Value) -> ToolOutcome {
        let arguments: UpdateTaskArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        match self.runtime.update_task(&arguments.id, arguments.status) {
            Ok(task) => ToolOutcome::text(format!("updated {} {}", task.id, task.status.as_str())),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

fn member_report(member: &TeamMember) -> ToolOutcome {
    ToolOutcome::text(format!("{}\n{}", member.status.as_str(), member.last_text))
}

fn past_deadline(now: Instant, deadline: Instant) -> bool {
    now >= deadline
}

#[derive(Deserialize)]
struct SpawnArguments {
    name: String,
    task: String,
}

#[derive(Deserialize)]
struct SendArguments {
    agent_id: String,
    message: String,
}

#[derive(Deserialize)]
struct WaitArguments {
    agent_id: String,
}

#[derive(Deserialize)]
struct CreateTaskArguments {
    title: String,
    description: String,
}

#[derive(Deserialize)]
struct UpdateTaskArguments {
    id: String,
    status: TaskStatus,
}

#[async_trait]
impl Tool for TeamTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.kind.name().to_string(),
            description: self.kind.description().to_string(),
            parameters: self.kind.parameters(),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        match self.kind {
            TeamToolKind::SpawnTeammate => self.spawn_teammate(context, arguments),
            TeamToolKind::ListAgents => self.list_agents(),
            TeamToolKind::SendMessage => self.send_message(context, arguments),
            TeamToolKind::WaitAgent => self.wait_agent(arguments).await,
            TeamToolKind::TaskCreate => self.create_task(arguments),
            TeamToolKind::TaskList => self.list_tasks(),
            TeamToolKind::TaskUpdate => self.update_task(arguments),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockStore, ScriptedProvider, config, context};
    use dsh_core::{FixedClock, SessionId, ToolRegistry};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn runtime_with_state(state_dir: PathBuf, cwd: &Path) -> Arc<TeamRuntime> {
        Arc::new(TeamRuntime::new(
            Arc::new(MockStore::default()),
            Arc::new(ScriptedProvider::new(Vec::new())),
            ToolRegistry::new(),
            config(cwd),
            Arc::new(FixedClock(1_000)),
            state_dir,
        ))
    }

    fn runtime(directory: &tempfile::TempDir) -> Arc<TeamRuntime> {
        runtime_with_state(directory.path().join("state"), directory.path())
    }

    fn find(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|tool| tool.schema().name == name)
            .expect("tool")
            .clone()
    }

    fn required(tool: &Arc<dyn Tool>) -> Vec<String> {
        tool.schema().parameters["required"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_runtime_exposes_the_seven_team_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(&directory);
        let tools = runtime.tools();
        let names: Vec<String> = tools.iter().map(|tool| tool.schema().name).collect();
        assert_eq!(
            names,
            vec![
                "spawn_teammate".to_string(),
                "list_agents".to_string(),
                "send_message".to_string(),
                "wait_agent".to_string(),
                "team_task_create".to_string(),
                "team_task_list".to_string(),
                "team_task_update".to_string(),
            ]
        );
        for tool in &tools {
            let schema = tool.schema();
            assert_eq!(schema.parameters["type"], "object", "{}", schema.name);
            assert!(
                schema.parameters["properties"].is_object(),
                "{}",
                schema.name
            );
            assert!(!schema.description.is_empty(), "{}", schema.name);
        }
        for (name, phrase) in [
            ("spawn_teammate", "Spawn a teammate"),
            ("list_agents", "List teammates"),
            ("send_message", "Send another turn"),
            ("wait_agent", "Wait up to 300 seconds"),
            ("team_task_create", "Add a task"),
            ("team_task_list", "List team tasks"),
            ("team_task_update", "Move one team task"),
        ] {
            assert!(
                find(&tools, name).schema().description.contains(phrase),
                "{name}"
            );
        }
    }

    #[test]
    fn schemas_declare_their_required_arguments() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(&directory);
        let tools = runtime.tools();

        assert_eq!(required(&find(&tools, "spawn_teammate")), ["name", "task"]);
        assert_eq!(
            required(&find(&tools, "send_message")),
            ["agent_id", "message"]
        );
        assert_eq!(required(&find(&tools, "wait_agent")), ["agent_id"]);
        assert_eq!(
            required(&find(&tools, "team_task_create")),
            ["title", "description"]
        );
        assert_eq!(
            required(&find(&tools, "team_task_update")),
            ["id", "status"]
        );
        assert_eq!(required(&find(&tools, "list_agents")), Vec::<String>::new());
        assert_eq!(
            required(&find(&tools, "team_task_list")),
            Vec::<String>::new()
        );

        assert_eq!(
            find(&tools, "team_task_update").schema().parameters["properties"]["status"]["enum"],
            json!(["pending", "in_progress", "completed"])
        );
    }

    #[tokio::test]
    async fn task_tools_create_list_and_update_durable_tasks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path().join("state");
        let runtime = runtime_with_state(state_dir.clone(), directory.path());
        let tools = runtime.tools();
        let tool_context = context(directory.path(), None);

        let create = find(&tools, "team_task_create");
        let outcome = create
            .run(
                &tool_context,
                r#"{"title":"  ship  ","description":"cut the release"}"#,
            )
            .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        let id = outcome
            .text_content()
            .strip_prefix("created ")
            .expect("created line")
            .to_string();
        assert!(id.starts_with("task-"));
        assert_eq!(runtime.state_path(), state_dir.join("team.json").as_path());
        assert!(runtime.state_path().exists());

        let list = find(&tools, "team_task_list");
        assert_eq!(
            list.run(&tool_context, "{}").await.text_content(),
            format!("{id}\tpending\tship")
        );

        let update = find(&tools, "team_task_update");
        let outcome = update
            .run(
                &tool_context,
                &format!(r#"{{"id":"{id}","status":"in_progress"}}"#),
            )
            .await;
        assert_eq!(outcome.text_content(), format!("updated {id} in_progress"));
        assert_eq!(
            list.run(&tool_context, "{}").await.text_content(),
            format!("{id}\tin_progress\tship")
        );

        let outcome = update
            .run(
                &tool_context,
                &format!(r#"{{"id":"{id}","status":"completed"}}"#),
            )
            .await;
        assert_eq!(outcome.text_content(), format!("updated {id} completed"));

        let restarted = runtime_with_state(state_dir, directory.path());
        let restarted_context = context(directory.path(), None);
        assert_eq!(
            find(&restarted.tools(), "team_task_list")
                .run(&restarted_context, "{}")
                .await
                .text_content(),
            format!("{id}\tcompleted\tship")
        );
    }

    #[tokio::test]
    async fn empty_lists_render_no_lines() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(&directory);
        let tools = runtime.tools();
        let context = context(directory.path(), None);
        assert_eq!(
            find(&tools, "list_agents")
                .run(&context, "{}")
                .await
                .text_content(),
            ""
        );
        assert_eq!(
            find(&tools, "team_task_list")
                .run(&context, "{}")
                .await
                .text_content(),
            ""
        );
    }

    #[tokio::test]
    async fn task_tools_reject_bad_input_and_unknown_ids() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(&directory);
        let tools = runtime.tools();
        let context = context(directory.path(), None);
        let create = find(&tools, "team_task_create");
        let update = find(&tools, "team_task_update");

        for title in ["", " ", "\\t"] {
            let outcome = create
                .run(
                    &context,
                    &format!(r#"{{"title":"{title}","description":""}}"#),
                )
                .await;
            assert!(outcome.is_error, "{title:?}");
            assert!(outcome.text_content().contains("title must not be empty"));
        }

        let outcome = update
            .run(&context, r#"{"id":"task-1","status":"done"}"#)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));

        let outcome = update
            .run(&context, r#"{"id":"task-1","status":"completed"}"#)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("unknown team task"));
    }

    #[tokio::test]
    async fn spawn_requires_a_durable_session_and_non_blank_fields() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(&directory);
        let tools = runtime.tools();
        let spawn = find(&tools, "spawn_teammate");

        let outcome = spawn
            .run(
                &context(directory.path(), None),
                r#"{"name":"alice","task":"work"}"#,
            )
            .await;
        assert_eq!(
            outcome.text_content(),
            "this tool requires a durable session"
        );

        let context = context(directory.path(), Some(SessionId::generate()));
        let outcome = spawn.run(&context, r#"{"name":"  ","task":"work"}"#).await;
        assert_eq!(outcome.text_content(), "teammate name must not be empty");
        let outcome = spawn.run(&context, r#"{"name":"alice","task":"  "}"#).await;
        assert_eq!(outcome.text_content(), "teammate task must not be empty");
    }

    #[tokio::test]
    async fn teammate_tools_reject_unknown_members() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(&directory);
        let tools = runtime.tools();
        let context = context(directory.path(), Some(SessionId::generate()));

        let outcome = find(&tools, "send_message")
            .run(&context, r#"{"agent_id":"member-1","message":"hello"}"#)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("unknown teammate"));

        let outcome = find(&tools, "wait_agent")
            .run(&context, r#"{"agent_id":"member-1"}"#)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("unknown teammate"));
    }

    #[tokio::test]
    async fn a_malformed_team_file_is_reported_and_left_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path().join("state");
        fs::create_dir_all(&state_dir).expect("state dir");
        let path = state_dir.join("team.json");
        let broken = b"{not json";
        fs::write(&path, broken).expect("write");
        let runtime = runtime_with_state(state_dir, directory.path());
        let tools = runtime.tools();
        let context = context(directory.path(), None);

        let outcome = find(&tools, "team_task_create")
            .run(&context, r#"{"title":"ship","description":""}"#)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("encoding failure"));
        assert_eq!(fs::read(&path).expect("read"), broken);

        let outcome = find(&tools, "team_task_list").run(&context, "{}").await;
        assert!(outcome.is_error);
        assert_eq!(fs::read(&path).expect("read"), broken);
    }

    #[test]
    fn the_wait_ceiling_is_300_seconds_and_the_boundary_is_inclusive() {
        assert_eq!(WAIT_CEILING, Duration::from_secs(300));
        let now = Instant::now();
        let deadline = now + WAIT_CEILING;
        assert!(!past_deadline(now, deadline));
        assert!(past_deadline(deadline, deadline));
        assert!(past_deadline(deadline + Duration::from_millis(1), deadline));
    }
}
