//! Fixtures shared by the crate's tests.

use crate::config::McpServerConfig;
use crate::connection::McpConnection;
use crate::error::McpError;
use crate::protocol::{McpToolResult, McpToolSpec};
use async_trait::async_trait;
use dsh_core::{
    Config, Env, EventRecord, LlmError, LlmErrorCode, LlmProvider, LlmRequest, LlmStream,
    PermissionMode, Plugin, PluginContext, PluginError, Runtime, SessionEvent, SessionHeader,
    SessionStore, SessionSummary, StoreError, SystemEnv, ToolContext, compose, ids::SessionId,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Environment access backed by a map.
#[derive(Default)]
pub struct MapEnv(HashMap<String, String>);

impl MapEnv {
    pub fn with(key: &str, value: &str) -> Self {
        Self(HashMap::from([(key.to_string(), value.to_string())]))
    }
}

impl Env for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// How a mock connection behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockMode {
    Ok,
    ListFails,
    CallFails,
    Panics,
    ConnectFails,
}

/// A connection that records calls and returns scripted outcomes.
pub struct MockConnection {
    specs: Vec<McpToolSpec>,
    mode: MockMode,
    calls: Mutex<Vec<(String, Value)>>,
}

impl MockConnection {
    pub fn new(specs: Vec<McpToolSpec>, mode: MockMode) -> Self {
        Self {
            specs,
            mode,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().expect("calls").clone()
    }
}

impl McpConnection for MockConnection {
    fn list_tools(&self) -> Result<Vec<McpToolSpec>, McpError> {
        if self.mode == MockMode::ListFails {
            Err(McpError::Protocol("list failed".to_string()))
        } else {
            Ok(self.specs.clone())
        }
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult, McpError> {
        self.calls
            .lock()
            .expect("calls")
            .push((name.to_string(), arguments));
        match self.mode {
            MockMode::Panics => panic!("mock connection panicked"),
            MockMode::CallFails => Err(McpError::Protocol("call failed".to_string())),
            _ => Ok(McpToolResult {
                text: format!("{name} result"),
                is_error: name == "bad",
            }),
        }
    }
}

/// A declaration with a stable shape.
pub fn spec(name: &str) -> McpToolSpec {
    McpToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        input_schema: json!({"type": "object", "properties": {}}),
    }
}

/// A tool context that permits no writes.
pub fn tool_context() -> ToolContext {
    ToolContext::new(
        std::env::temp_dir(),
        PermissionMode::ReadOnly,
        CancellationToken::new(),
    )
}

/// Path the lingering test server writes its pid to.
pub fn pid_path(directory: &Path) -> PathBuf {
    directory.join("server.pid")
}

/// A shell MCP server that answers the initialize, list, and call frames.
pub fn stub_server(directory: &Path, name: &str) -> McpServerConfig {
    let script = directory.join("stub-mcp.sh");
    std::fs::write(&script, STUB_SERVER).expect("write stub server");
    McpServerConfig {
        name: name.to_string(),
        command: "bash".to_string(),
        args: vec![script.to_string_lossy().into_owned()],
        env: Vec::new(),
    }
}

/// A shell MCP server that stays alive after its stdin closes.
pub fn lingering_server(directory: &Path) -> McpServerConfig {
    let script = directory.join("lingering-mcp.sh");
    std::fs::write(&script, LINGERING_SERVER).expect("write lingering server");
    McpServerConfig {
        name: "lingering".to_string(),
        command: "bash".to_string(),
        args: vec![script.to_string_lossy().into_owned()],
        env: vec![(
            "PIDFILE".to_string(),
            pid_path(directory).to_string_lossy().into_owned(),
        )],
    }
}

const STUB_SERVER: &str = r#"while IFS= read -r -t 10 line; do
  case "$line" in
    *'"id":1'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"stub","version":"1.0.0"}}}'
      ;;
    *'"id":2'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
      ;;
    *'"id":3'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"${STUB_TEXT:-stub }\"},{\"type\":\"text\",\"text\":\"result\"}],\"isError\":false}}"
      ;;
    *'"id":4'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"${STUB_TEXT:-stub }\"},{\"type\":\"text\",\"text\":\"result\"}],\"isError\":false}}"
      ;;
  esac
done
"#;

const LINGERING_SERVER: &str = r#"printf '%s\n' "$$" > "$PIDFILE"
while IFS= read -r -t 10 line; do
  case "$line" in
    *'"id":1'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
  esac
done
if [ "$?" -eq 1 ]; then
  sleep 30
fi
"#;

pub struct NoStore;

impl SessionStore for NoStore {
    fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
        Ok(())
    }

    fn append(
        &self,
        _session_id: &SessionId,
        _at: i64,
        _event: SessionEvent,
    ) -> Result<EventRecord, StoreError> {
        Err(StoreError::Backend("unused".to_string()))
    }

    fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
        Ok(Vec::new())
    }

    fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        Ok(Vec::new())
    }

    fn has(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
        Ok(false)
    }

    fn delete(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
        Ok(false)
    }
}

pub struct NoProvider;

#[async_trait]
impl LlmProvider for NoProvider {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> Result<LlmStream, LlmError> {
        Err(LlmError::new(LlmErrorCode::Transport, "unused", false))
    }
}

pub struct StorePlugin;

impl Plugin for StorePlugin {
    fn name(&self) -> &'static str {
        "store"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.set_store(Arc::new(NoStore))
    }
}

pub struct ProviderPlugin;

impl Plugin for ProviderPlugin {
    fn name(&self) -> &'static str {
        "provider"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.set_provider(Arc::new(NoProvider))
    }
}

/// Compose the fixtures with one extra plugin.
pub fn compose_with(plugin: Box<dyn Plugin>) -> Result<Runtime, dsh_core::ComposeError> {
    let plugins: Vec<Box<dyn Plugin>> =
        vec![Box::new(StorePlugin), Box::new(ProviderPlugin), plugin];
    compose(&plugins, &Config::default(), &SystemEnv)
}
