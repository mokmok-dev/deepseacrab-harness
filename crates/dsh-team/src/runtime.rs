//! The process-local runtime behind the team tools.

use crate::error::TeamError;
use crate::file::TeamFile;
use crate::model::{MemberStatus, TaskStatus, TeamMember, TeamState, TeamTask};
use crate::tools::TeamTool;
use dsh_agent::{AgentConfig, AgentError, AgentLoop, RunOutcome};
use dsh_core::{Clock, LlmProvider, SessionId, SessionStore, Tool, ToolRegistry};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Tools a teammate must not see: delegation would nest teams, and the team
/// tools would let a child rewrite the roster that owns it.
const EXCLUDED_CHILD_TOOLS: [&str; 10] = [
    "subagent",
    "workflow",
    "ralph",
    "spawn_teammate",
    "list_agents",
    "send_message",
    "wait_agent",
    "team_task_create",
    "team_task_list",
    "team_task_update",
];

/// Durable team state plus the seams a teammate turn runs on.
pub struct TeamRuntime {
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AgentConfig,
    clock: Arc<dyn Clock>,
    file: TeamFile,
    state: Arc<Mutex<TeamState>>,
}

impl TeamRuntime {
    /// Build a runtime over one state directory.
    ///
    /// The team file is read lazily, so a malformed file surfaces from the
    /// first tool call instead of construction and is never overwritten.
    #[must_use]
    pub fn new(
        store: Arc<dyn SessionStore>,
        provider: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        config: AgentConfig,
        clock: Arc<dyn Clock>,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            provider,
            tools,
            config,
            clock,
            file: TeamFile::in_dir(state_dir),
            state: Arc::new(Mutex::new(TeamState::default())),
        }
    }

    /// The seven model-facing team tools, sharing this runtime's state.
    #[must_use]
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        TeamTool::all(self)
    }

    /// The persisted team file.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        self.file.path()
    }

    fn lock(&self) -> MutexGuard<'_, TeamState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn load(&self) -> Result<TeamState, TeamError> {
        let mut guard = self.lock();
        let state = self.file.load()?;
        *guard = state.clone();
        drop(guard);
        Ok(state)
    }

    fn update<T>(
        &self,
        change: impl FnOnce(&mut TeamState) -> Result<T, TeamError>,
    ) -> Result<T, TeamError> {
        let mut guard = self.lock();
        let mut state = self.file.load()?;
        let result = change(&mut state)?;
        self.file.save(&state)?;
        *guard = state;
        drop(guard);
        Ok(result)
    }

    pub(crate) fn list_members(&self) -> Result<Vec<TeamMember>, TeamError> {
        Ok(self.load()?.members)
    }

    pub(crate) fn list_tasks(&self) -> Result<Vec<TeamTask>, TeamError> {
        Ok(self.load()?.tasks)
    }

    pub(crate) fn member(&self, id: &str) -> Result<TeamMember, TeamError> {
        self.load()?
            .members
            .into_iter()
            .find(|member| member.id == id)
            .ok_or_else(|| TeamError::UnknownMember(id.to_string()))
    }

    pub(crate) fn add_member(&self, name: &str) -> Result<TeamMember, TeamError> {
        let member = TeamMember::mint(name);
        member.validate()?;
        self.update(|state| {
            state.members.push(member.clone());
            Ok(())
        })?;
        Ok(member)
    }

    pub(crate) fn message_member(&self, id: &str, message: &str) -> Result<TeamMember, TeamError> {
        if message.trim().is_empty() {
            return Err(TeamError::Invalid("message must not be empty".to_string()));
        }
        self.update(|state| {
            let member = state
                .members
                .iter_mut()
                .find(|member| member.id == id)
                .ok_or_else(|| TeamError::UnknownMember(id.to_string()))?;
            if member.status == MemberStatus::Running {
                return Err(TeamError::Running(id.to_string()));
            }
            member.status = MemberStatus::Running;
            Ok(member.clone())
        })
    }

    pub(crate) fn create_task(
        &self,
        title: &str,
        description: &str,
    ) -> Result<TeamTask, TeamError> {
        let task = TeamTask::mint(title, description);
        task.validate()?;
        self.update(|state| {
            state.tasks.push(task.clone());
            Ok(())
        })?;
        Ok(task)
    }

    pub(crate) fn update_task(&self, id: &str, status: TaskStatus) -> Result<TeamTask, TeamError> {
        self.update(|state| {
            let task = state
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .ok_or_else(|| TeamError::UnknownTask(id.to_string()))?;
            task.status = status;
            Ok(task.clone())
        })
    }

    /// Build the loop for one teammate turn.
    pub(crate) fn child_agent(
        &self,
        cwd: PathBuf,
        parent: Option<SessionId>,
    ) -> Result<AgentLoop, AgentError> {
        let mut config = self.config.clone();
        config.cwd = cwd;
        config.parent_session = parent;
        AgentLoop::new(
            Arc::clone(&self.store),
            Arc::clone(&self.provider),
            self.child_tools(),
            config,
            Arc::clone(&self.clock),
        )
    }

    fn child_tools(&self) -> ToolRegistry {
        let mut tools = self.tools.clone();
        for name in EXCLUDED_CHILD_TOOLS {
            tools.remove(name);
        }
        tools
    }

    /// Settle one background turn into the member record.
    pub(crate) fn settle(&self, member_id: &str, result: Result<RunOutcome, AgentError>) {
        let (status, text) = match result {
            Ok(outcome) if outcome.is_success() => (MemberStatus::Idle, outcome.text),
            Ok(outcome) => (MemberStatus::Failed, outcome.text),
            Err(error) => (MemberStatus::Failed, error.to_string()),
        };
        let _ = self.finish_member(member_id, status, text);
    }

    fn finish_member(&self, id: &str, status: MemberStatus, text: String) -> Result<(), TeamError> {
        self.update(|state| {
            let member = state
                .members
                .iter_mut()
                .find(|member| member.id == id)
                .ok_or_else(|| TeamError::UnknownMember(id.to_string()))?;
            member.status = status;
            member.last_text = text;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        MockStore, NamedTool, ScriptedProvider, config, context, text_response,
    };
    use dsh_core::{FixedClock, SessionEvent, Tool, ToolRegistry};
    use std::fs;

    fn tool(runtime: &Arc<TeamRuntime>, name: &str) -> Arc<dyn Tool> {
        runtime
            .tools()
            .into_iter()
            .find(|tool| tool.schema().name == name)
            .expect("tool")
    }

    fn agent_arguments(agent_id: &str) -> String {
        serde_json::json!({"agent_id": agent_id}).to_string()
    }

    fn message_arguments(agent_id: &str, message: &str) -> String {
        serde_json::json!({"agent_id": agent_id, "message": message}).to_string()
    }

    fn user_texts(store: &MockStore, session: &SessionId) -> Vec<String> {
        store
            .load(session)
            .expect("load")
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::UserMessage { message } => Some(message.text_content()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_runtime_reads_a_missing_team_file_as_empty() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path().join("state");
        let runtime = TeamRuntime::new(
            Arc::new(MockStore::default()),
            Arc::new(ScriptedProvider::new(Vec::new())),
            ToolRegistry::new(),
            config(directory.path()),
            Arc::new(FixedClock(1_000)),
            state_dir.clone(),
        );
        assert_eq!(runtime.list_members().expect("members"), Vec::new());
        assert_eq!(runtime.list_tasks().expect("tasks"), Vec::new());
        assert_eq!(runtime.state_path(), state_dir.join("team.json").as_path());
    }

    #[tokio::test(start_paused = true)]
    async fn teammates_run_child_sessions_and_take_follow_up_messages() {
        let directory = tempfile::tempdir().expect("tempdir");
        let cwd = directory.path().join("work");
        fs::create_dir_all(&cwd).expect("cwd");
        let store = Arc::new(MockStore::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            text_response("child result"),
            text_response("second result"),
        ]));
        let mut registry = ToolRegistry::new();
        for name in EXCLUDED_CHILD_TOOLS {
            registry
                .insert(Arc::new(NamedTool { name }))
                .expect("excluded tool");
        }
        registry
            .insert(Arc::new(NamedTool { name: "echo" }))
            .expect("echo tool");
        let runtime = Arc::new(TeamRuntime::new(
            store.clone(),
            provider.clone(),
            registry,
            config(&cwd),
            Arc::new(FixedClock(1_000)),
            directory.path().join("state"),
        ));
        let parent = SessionId::generate();
        let context = context(&cwd, Some(parent.clone()));

        let outcome = tool(&runtime, "spawn_teammate")
            .run(&context, r#"{"name":"alice","task":"first task"}"#)
            .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        let spawned = outcome.text_content();
        let rest = spawned.strip_prefix("spawned ").expect("spawned prefix");
        let (member_id, child) = rest.split_once(" session ").expect("session part");
        let member_id = member_id.to_string();
        let child = SessionId::parse(child).expect("child session");

        let outcome = tool(&runtime, "wait_agent")
            .run(&context, &agent_arguments(&member_id))
            .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), "idle\nchild result");

        let members = runtime.list_members().expect("members");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, member_id);
        assert_eq!(members[0].name, "alice");
        assert_eq!(members[0].status, MemberStatus::Idle);
        assert_eq!(members[0].last_text, "child result");
        assert_eq!(members[0].session_id, child);
        assert_eq!(
            store.header(&child).expect("header").parent_session,
            Some(parent)
        );
        assert_eq!(user_texts(&store, &child), vec!["first task"]);

        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        let child_tools: Vec<&str> = requests[0]
            .tools
            .iter()
            .map(|schema| schema.name.as_str())
            .collect();
        assert_eq!(child_tools, vec!["echo"]);

        let outcome = tool(&runtime, "send_message")
            .run(&context, &message_arguments(&member_id, "second task"))
            .await;
        assert_eq!(outcome.text_content(), format!("sent {member_id}"));

        let outcome = tool(&runtime, "wait_agent")
            .run(&context, &agent_arguments(&member_id))
            .await;
        assert_eq!(outcome.text_content(), "idle\nsecond result");

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            user_texts(&store, &child),
            vec!["first task", "second task"]
        );
        assert_eq!(
            runtime.member(&member_id).expect("member").last_text,
            "second result"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_child_turns_mark_the_member_failed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(MockStore::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let runtime = Arc::new(TeamRuntime::new(
            store,
            provider,
            ToolRegistry::new(),
            config(directory.path()),
            Arc::new(FixedClock(1_000)),
            directory.path().join("state"),
        ));
        let context = context(directory.path(), Some(SessionId::generate()));

        let outcome = tool(&runtime, "spawn_teammate")
            .run(&context, r#"{"name":"alice","task":"doomed"}"#)
            .await;
        let spawned = outcome.text_content();
        let member_id = spawned
            .strip_prefix("spawned ")
            .expect("spawned prefix")
            .split_once(" session ")
            .expect("session part")
            .0
            .to_string();

        let outcome = tool(&runtime, "wait_agent")
            .run(&context, &agent_arguments(&member_id))
            .await;
        assert_eq!(outcome.text_content(), "failed\n");
        let member = runtime.member(&member_id).expect("member");
        assert_eq!(member.status, MemberStatus::Failed);
        assert_eq!(member.last_text, "");
    }

    #[tokio::test(start_paused = true)]
    async fn an_invalid_provider_name_fails_the_member() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(MockStore::default());
        let runtime = Arc::new(TeamRuntime::new(
            store,
            Arc::new(crate::test_support::NamelessProvider),
            ToolRegistry::new(),
            config(directory.path()),
            Arc::new(FixedClock(1_000)),
            directory.path().join("state"),
        ));
        let context = context(directory.path(), Some(SessionId::generate()));

        let outcome = tool(&runtime, "spawn_teammate")
            .run(&context, r#"{"name":"alice","task":"doomed"}"#)
            .await;
        let member_id = outcome
            .text_content()
            .strip_prefix("spawned ")
            .expect("spawned prefix")
            .split_once(" session ")
            .expect("session part")
            .0
            .to_string();

        let outcome = tool(&runtime, "wait_agent")
            .run(&context, &agent_arguments(&member_id))
            .await;
        assert_eq!(
            outcome.text_content(),
            "failed\nprovider name \"\" is not a valid identifier"
        );
    }

    #[test]
    fn running_members_refuse_new_messages() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = TeamRuntime::new(
            Arc::new(MockStore::default()),
            Arc::new(ScriptedProvider::new(Vec::new())),
            ToolRegistry::new(),
            config(directory.path()),
            Arc::new(FixedClock(1_000)),
            directory.path().join("state"),
        );
        let member = runtime.add_member("alice").expect("member");
        assert!(matches!(
            runtime.message_member(&member.id, "hello"),
            Err(TeamError::Running(_))
        ));
        assert!(matches!(
            runtime.message_member(&member.id, " \t\n"),
            Err(TeamError::Invalid(_))
        ));
    }
}
