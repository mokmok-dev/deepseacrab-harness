//! The tool seam: schemas shown to the model and execution behind one call.

use crate::event::{EventRecord, SessionEvent};
use crate::ids::SessionId;
use crate::message::ContentBlock;
use crate::permission::{PathError, ensure_within};
use crate::store::StoreError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// How much the harness may change on the caller's machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Reads are allowed; writes, commands, and network effects are refused.
    ReadOnly,
    /// Writes are confined to the session working directory.
    WorkspaceWrite,
    /// No path or command restriction.
    DangerFullAccess,
}

impl PermissionMode {
    /// Parse the spelling accepted by the CLI and configuration file.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    /// The stable CLI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// Whether the mode permits a write at all.
    #[must_use]
    pub const fn allows_write(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

/// The model-facing declaration of one tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Registered name the model calls.
    pub name: String,
    /// When and how to call it.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

impl ToolSchema {
    /// Build a schema, rejecting an empty name.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Result<Self, ToolRegistryError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ToolRegistryError::EmptyName);
        }
        Ok(Self {
            name,
            description: description.into(),
            parameters,
        })
    }
}

/// Durable session access available to session-owned tools.
pub trait ToolSession: Send + Sync {
    /// Session receiving the tool's facts.
    fn session_id(&self) -> &SessionId;

    /// Append one durable fact.
    fn append(&self, event: SessionEvent) -> Result<EventRecord, StoreError>;

    /// Current wall clock in Unix epoch milliseconds.
    fn now_millis(&self) -> i64;

    /// Search durable event payloads.
    fn search(
        &self,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<crate::session::SessionSearchHit>, StoreError> {
        Err(StoreError::Unsupported("session search".to_string()))
    }
}

/// Everything one tool invocation may observe and use.
#[derive(Clone)]
pub struct ToolContext {
    /// Physical session working directory.
    pub cwd: PathBuf,
    /// Harness state directory for durable non-session files.
    pub state_dir: PathBuf,
    /// Effective permission mode.
    pub permission: PermissionMode,
    /// Cancellation propagated to the tool body.
    pub cancel: CancellationToken,
    /// Durable session access for session-owned tools.
    pub session: Option<Arc<dyn ToolSession>>,
}

impl ToolContext {
    /// Build a context for a session.
    #[must_use]
    pub fn new(cwd: PathBuf, permission: PermissionMode, cancel: CancellationToken) -> Self {
        Self {
            state_dir: cwd.clone(),
            cwd,
            permission,
            cancel,
            session: None,
        }
    }

    /// Set the harness state directory.
    #[must_use]
    pub fn with_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.state_dir = state_dir;
        self
    }

    /// Attach the durable session handle.
    #[must_use]
    pub fn with_session(mut self, session: Arc<dyn ToolSession>) -> Self {
        self.session = Some(session);
        self
    }

    /// The durable session handle, when one was attached.
    #[must_use]
    pub fn session(&self) -> Option<&Arc<dyn ToolSession>> {
        self.session.as_ref()
    }

    /// Resolve a path and enforce the session's write policy.
    ///
    /// The check runs after lexical normalization, so `..` cannot climb out of
    /// the workspace by being spelled literally.
    pub fn writable_path(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        let path = crate::permission::resolve_path(&self.cwd, raw)?;
        ensure_within(&self.cwd, &path, self.permission)?;
        Ok(path)
    }

    /// Resolve a path for reading, refusing traversal outside the workspace
    /// unless full access was granted.
    pub fn readable_path(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        let path = crate::permission::resolve_path(&self.cwd, raw)?;
        if self.permission == PermissionMode::DangerFullAccess {
            return Ok(path);
        }
        if path.starts_with(&self.cwd) {
            Ok(path)
        } else {
            Err(ToolFailure::Permission(PathError::OutsideWorkspace {
                path,
                cwd: self.cwd.clone(),
            }))
        }
    }
}

/// Why a tool refused to act.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolFailure {
    /// The path policy rejected the request.
    #[error(transparent)]
    Permission(#[from] PathError),
    /// The tool input was malformed.
    #[error("{0}")]
    InvalidArguments(String),
    /// The tool body failed.
    #[error("{0}")]
    Execution(String),
}

/// The normalized result of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Model-facing content.
    pub content: Vec<ContentBlock>,
    /// Whether the call failed.
    pub is_error: bool,
    /// Tool-private presentation payload.
    pub meta: Option<Value>,
}

impl ToolOutcome {
    /// A successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: false,
            meta: None,
        }
    }

    /// A failed text result.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: true,
            meta: None,
        }
    }

    /// Add a presentation payload.
    #[must_use]
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }

    /// The concatenated text content.
    #[must_use]
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => None,
            })
            .collect()
    }
}

/// A capability the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The declaration shown to the model.
    fn schema(&self) -> ToolSchema;

    /// Whether this tool may run beside sibling calls in one batch.
    fn concurrency_safe(&self) -> bool {
        false
    }

    /// Execute one call. Failures are returned as an error outcome so the loop
    /// can always settle the matching tool result.
    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome;

    /// Run a call with argument parsing and panic containment applied.
    ///
    /// The loop uses this entry point; direct callers use [`Tool::execute`].
    async fn run(&self, context: &ToolContext, arguments: &str) -> ToolOutcome {
        let parsed: Value = match serde_json::from_str::<Value>(arguments) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return ToolOutcome::error("tool arguments must be a JSON object");
            }
            Err(error) => {
                return ToolOutcome::error(format!("tool arguments are not valid JSON: {error}"));
            }
        };
        let name = self.schema().name;
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            self.execute(context, parsed),
        ))
        .await
        .unwrap_or_else(|_| ToolOutcome::error(format!("tool {name} panicked")))
    }
}

/// Registry failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolRegistryError {
    /// A tool declared no name.
    #[error("tool name must not be empty")]
    EmptyName,
    /// Two tools claimed the same name.
    #[error("tool {0} is already registered")]
    Duplicate(String),
}

/// Name-keyed tool collection with deterministic schema order.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool.
    pub fn insert(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let name = tool.schema().name;
        if self.tools.contains_key(&name) {
            return Err(ToolRegistryError::Duplicate(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Look up a tool by its registered name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Every declaration, sorted by name.
    #[must_use]
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    /// Every registered name, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Remove a tool by name, returning it when present.
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Test-only tool used by the registry and plugin tests.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct EchoTool {
    pub(crate) name: String,
}

#[cfg(test)]
#[async_trait]
impl Tool for EchoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(&self.name, "echo", serde_json::json!({"type": "object"}))
            .expect("valid schema")
    }

    async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
        ToolOutcome::text(arguments.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::EventSeq;
    use proptest::prelude::*;
    use std::path::Path;

    fn context(mode: PermissionMode, cwd: &Path) -> ToolContext {
        ToolContext::new(cwd.to_path_buf(), mode, CancellationToken::new())
    }

    proptest! {
        #[test]
        fn permission_modes_round_trip(raw in prop_oneof![
            Just("read-only".to_string()),
            Just("workspace-write".to_string()),
            Just("danger-full-access".to_string()),
        ]) {
            let mode = PermissionMode::parse(&raw).expect("known mode");
            prop_assert_eq!(mode.as_str(), raw.as_str());
        }
    }

    #[test]
    fn unknown_permission_modes_are_rejected() {
        for raw in ["", "read", "workspace", "danger-full", "READ-ONLY"] {
            assert_eq!(PermissionMode::parse(raw), None);
        }
    }

    #[test]
    fn read_only_refuses_writes() {
        assert!(!PermissionMode::ReadOnly.allows_write());
        assert!(PermissionMode::WorkspaceWrite.allows_write());
        assert!(PermissionMode::DangerFullAccess.allows_write());
    }

    #[test]
    fn registry_rejects_duplicate_names() {
        let mut registry = ToolRegistry::new();
        registry
            .insert(Arc::new(EchoTool {
                name: "echo".to_string(),
            }))
            .expect("first insert");
        assert_eq!(
            registry
                .insert(Arc::new(EchoTool {
                    name: "echo".to_string(),
                }))
                .expect_err("duplicate"),
            ToolRegistryError::Duplicate("echo".to_string())
        );
    }

    #[test]
    fn registry_orders_schemas_by_name() {
        let mut registry = ToolRegistry::new();
        for name in ["zebra", "alpha", "middle"] {
            registry
                .insert(Arc::new(EchoTool {
                    name: name.to_string(),
                }))
                .expect("insert");
        }
        assert_eq!(registry.names(), vec!["alpha", "middle", "zebra"]);
        assert_eq!(
            registry
                .schemas()
                .iter()
                .map(|schema| schema.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "middle", "zebra"]
        );
        assert_eq!(registry.len(), 3);
        assert!(!registry.is_empty());
        assert_eq!(
            registry.get("alpha").expect("present").schema().name,
            "alpha"
        );
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn registry_removal_returns_the_tool_once() {
        let mut registry = ToolRegistry::new();
        registry
            .insert(Arc::new(EchoTool {
                name: "echo".to_string(),
            }))
            .expect("insert");
        assert!(registry.remove("echo").is_some());
        assert!(registry.remove("echo").is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn new_registries_are_empty() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.schemas(), Vec::new());
    }

    #[test]
    fn context_enforces_the_workspace_boundary() {
        let cwd = std::env::temp_dir().join("dsh-core-context-policy");
        let workspace = context(PermissionMode::WorkspaceWrite, &cwd);
        assert_eq!(
            workspace.writable_path("file.txt").expect("writable"),
            cwd.join("file.txt")
        );
        assert!(workspace.writable_path("../escape").is_err());
        assert_eq!(
            workspace.readable_path("file.txt").expect("readable"),
            cwd.join("file.txt")
        );
        assert!(workspace.readable_path("/etc/passwd").is_err());

        let full = context(PermissionMode::DangerFullAccess, &cwd);
        assert_eq!(
            full.readable_path("/etc/passwd").expect("full access"),
            PathBuf::from("/etc/passwd")
        );
    }

    #[test]
    fn tools_are_exclusive_unless_they_opt_in() {
        struct PlainTool;
        #[async_trait]
        impl Tool for PlainTool {
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("plain", "p", serde_json::json!({})).expect("schema")
            }
            async fn execute(&self, _context: &ToolContext, _arguments: Value) -> ToolOutcome {
                ToolOutcome::text("plain")
            }
        }
        struct SafeTool;
        #[async_trait]
        impl Tool for SafeTool {
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("safe", "s", serde_json::json!({})).expect("schema")
            }
            fn concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(&self, _context: &ToolContext, _arguments: Value) -> ToolOutcome {
                ToolOutcome::text("safe")
            }
        }
        assert!(!PlainTool.concurrency_safe());
        assert!(SafeTool.concurrency_safe());
    }

    #[test]
    fn session_handles_attach_to_tool_contexts() {
        struct MockSession {
            id: SessionId,
            events: std::sync::Mutex<Vec<SessionEvent>>,
        }
        impl ToolSession for MockSession {
            fn session_id(&self) -> &SessionId {
                &self.id
            }
            fn append(&self, event: SessionEvent) -> Result<EventRecord, StoreError> {
                self.events.lock().expect("events").push(event.clone());
                Ok(EventRecord {
                    seq: EventSeq::new(0),
                    at: 1,
                    event,
                })
            }
            fn now_millis(&self) -> i64 {
                1
            }
        }
        let session = Arc::new(MockSession {
            id: SessionId::generate(),
            events: std::sync::Mutex::new(Vec::new()),
        });
        let context = ToolContext::new(
            PathBuf::from("/work"),
            PermissionMode::WorkspaceWrite,
            CancellationToken::new(),
        )
        .with_session(session.clone());
        assert_eq!(
            context.session().expect("session").session_id(),
            &session.id
        );
        assert_eq!(context.session().expect("session").now_millis(), 1);
        let context = context.with_state_dir(PathBuf::from("/state"));
        assert_eq!(context.state_dir, PathBuf::from("/state"));
        assert_eq!(
            context.session().expect("session").search("query", 1),
            Err(StoreError::Unsupported("session search".to_string()))
        );
        let event = SessionEvent::WorkingDirectoryChanged {
            cwd: "/work".to_string(),
        };
        context
            .session()
            .expect("session")
            .append(event.clone())
            .expect("append");
        assert_eq!(*session.events.lock().expect("events"), vec![event]);
    }

    #[test]
    fn empty_schema_names_are_rejected() {
        assert_eq!(
            ToolSchema::new("", "d", serde_json::json!({})),
            Err(ToolRegistryError::EmptyName)
        );
    }

    #[tokio::test]
    async fn run_rejects_non_object_arguments() {
        let tool = EchoTool {
            name: "echo".to_string(),
        };
        let outcome = tool
            .run(&context(PermissionMode::ReadOnly, Path::new("/tmp")), "[]")
            .await;
        assert!(outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "tool arguments must be a JSON object"
        );
    }

    #[tokio::test]
    async fn run_rejects_malformed_arguments() {
        let tool = EchoTool {
            name: "echo".to_string(),
        };
        let outcome = tool
            .run(&context(PermissionMode::ReadOnly, Path::new("/tmp")), "{")
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("not valid JSON"));
    }

    #[tokio::test]
    async fn run_reports_panics_as_tool_errors() {
        struct PanicTool;
        #[async_trait]
        impl Tool for PanicTool {
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("panic", "p", serde_json::json!({})).expect("schema")
            }
            async fn execute(&self, _context: &ToolContext, _arguments: Value) -> ToolOutcome {
                panic!("boom");
            }
        }
        let outcome = PanicTool
            .run(&context(PermissionMode::ReadOnly, Path::new("/tmp")), "{}")
            .await;
        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), "tool panic panicked");
    }

    #[test]
    fn tool_outcome_carries_text_and_meta() {
        let outcome = ToolOutcome::text("ok").with_meta(serde_json::json!({"lines": 1}));
        assert!(!outcome.is_error);
        assert_eq!(outcome.text_content(), "ok");
        assert_eq!(outcome.meta, Some(serde_json::json!({"lines": 1})));
    }
}
