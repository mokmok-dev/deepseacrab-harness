//! Fixtures shared by the crate's tests.

use crate::config::LspServerConfig;
use crate::connection::LspConnection;
use crate::error::LspError;
use async_trait::async_trait;
use dsh_core::{
    ComposeError, Config, Env, EventRecord, LlmError, LlmErrorCode, LlmProvider, LlmRequest,
    LlmStream, Plugin, PluginContext, PluginError, Runtime, SessionEvent, SessionHeader,
    SessionStore, SessionSummary, StoreError, SystemEnv, compose, ids::SessionId,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Run blocking transport work with a wall-clock ceiling.
///
/// A stalled server must fail the test quickly rather than hang the suite.
pub fn run_blocking<T, F>(work: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("blocking lsp work did not finish")
}

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
    OpenFails,
    RequestFails,
    Panics,
    ConnectFails,
}

/// A connection that records calls and returns scripted LSP payloads.
pub struct MockConnection {
    mode: MockMode,
    hover: Value,
    opens: Mutex<Vec<(String, String, String)>>,
    requests: Mutex<Vec<(String, Value)>>,
}

impl MockConnection {
    pub fn new(mode: MockMode) -> Self {
        Self {
            mode,
            hover: json!({"contents": {"kind": "markdown", "value": "mock hover"}}),
            opens: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn with_hover(hover: Value) -> Self {
        Self {
            hover,
            ..Self::new(MockMode::Ok)
        }
    }

    pub fn opens(&self) -> Vec<(String, String, String)> {
        self.opens.lock().expect("opens").clone()
    }

    pub fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().expect("requests").clone()
    }

    fn result_for(&self, method: &str) -> Value {
        match method {
            "textDocument/hover" => self.hover.clone(),
            "textDocument/definition" => json!([{
                "uri": "file:///tmp/mock.rs",
                "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 9}}
            }]),
            "textDocument/references" => json!([
                {"uri": "file:///tmp/mock.rs", "range": {"start": {"line": 2, "character": 4}}},
                {"uri": "file:///tmp/mock.rs", "range": {"start": {"line": 8, "character": 0}}}
            ]),
            "textDocument/documentSymbol" => json!([{
                "name": "main",
                "kind": 12,
                "children": [{"name": "local", "kind": 13}]
            }]),
            _ => Value::Null,
        }
    }
}

impl LspConnection for MockConnection {
    fn open(&self, path: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        self.opens.lock().expect("opens").push((
            path.to_string(),
            language_id.to_string(),
            text.to_string(),
        ));
        if self.mode == MockMode::OpenFails {
            return Err(LspError::Protocol("mock open failed".to_string()));
        }
        Ok(())
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.requests
            .lock()
            .expect("requests")
            .push((method.to_string(), params));
        match self.mode {
            MockMode::Panics => panic!("mock connection panicked"),
            MockMode::RequestFails => Err(LspError::Protocol("mock request failed".to_string())),
            _ => Ok(self.result_for(method)),
        }
    }
}

/// Path the lingering test server writes its pid to.
pub fn pid_path(directory: &Path) -> PathBuf {
    directory.join("server.pid")
}

/// A shell language server that answers initialize and hover frames.
pub fn stub_server(directory: &Path, name: &str) -> LspServerConfig {
    let script = directory.join("stub-lsp.sh");
    std::fs::write(&script, STUB_SERVER).expect("write stub server");
    LspServerConfig {
        name: name.to_string(),
        command: "bash".to_string(),
        args: vec![script.to_string_lossy().into_owned()],
        extensions: vec!["rs".to_string()],
    }
}

/// A shell language server that stays alive after its stdin closes.
pub fn lingering_server(directory: &Path) -> LspServerConfig {
    let script = directory.join("lingering-lsp.sh");
    std::fs::write(&script, LINGERING_SERVER).expect("write lingering server");
    LspServerConfig {
        name: "lingering".to_string(),
        command: "bash".to_string(),
        args: vec![script.to_string_lossy().into_owned()],
        extensions: vec!["rs".to_string()],
    }
}

/// A shell language server that stalls a request behind a flood of notifications.
pub fn noisy_server(directory: &Path, name: &str) -> LspServerConfig {
    let script = directory.join("noisy-lsp.sh");
    std::fs::write(&script, NOISY_SERVER).expect("write noisy server");
    LspServerConfig {
        name: name.to_string(),
        command: "bash".to_string(),
        args: vec![script.to_string_lossy().into_owned()],
        extensions: vec!["rs".to_string()],
    }
}

const NOISY_SERVER: &str = r#"send() {
  printf 'Content-Length: %s\r\n\r\n%s' "${#1}" "$1"
}
while IFS= read -r -t 5 header; do
  header="${header%$'\r'}"
  case "$header" in
    Content-Length:*) length="${header#Content-Length: }" ;;
    "")
      IFS= read -r -N "$length" body || true
      case "$body" in
        *'"method":"initialize"'*)
          send '{"jsonrpc":"2.0","id":1,"result":{}}'
          ;;
        *'"method":"textDocument/hover"'*)
          count=0
          while [ "$count" -lt 1024 ]; do
            send '{"jsonrpc":"2.0","method":"window/logMessage","params":{}}'
            count=$((count + 1))
          done
          send '{"jsonrpc":"2.0","id":2,"result":{}}'
          ;;
      esac
      ;;
  esac
done
"#;

const STUB_SERVER: &str = r#"send() {
  printf 'Content-Length: %s\r\n\r\n%s' "${#1}" "$1"
}
opened=0
while IFS= read -r -t 2 header || [ -n "$header" ]; do
  header="${header%$'\r'}"
  case "$header" in
    Content-Length:*) length="${header#Content-Length: }" ;;
    "")
      IFS= read -r -N "$length" body || true
      case "$body" in
        *'"method":"initialize"'*)
          send '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{},"serverInfo":{"name":"stub","version":"1.0.0"}}}'
          ;;
        *'"method":"textDocument/didOpen"'*)
          opened=1
          ;;
        *'"method":"textDocument/hover"'*)
          if [ "$opened" -eq 1 ]; then
            send '{"jsonrpc":"2.0","id":2,"result":{"contents":{"kind":"markdown","value":"stub hover"}}}'
          else
            send '{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"document not open"}}'
          fi
          ;;
      esac
      ;;
  esac
done
"#;

const LINGERING_SERVER: &str = r#"send() {
  printf 'Content-Length: %s\r\n\r\n%s' "${#1}" "$1"
}
printf '%s\n' "$$" > "$(dirname "$0")/server.pid"
while IFS= read -r -t 5 header; do
  header="${header%$'\r'}"
  case "$header" in
    Content-Length:*) length="${header#Content-Length: }" ;;
    "")
      IFS= read -r -N "$length" body || true
      case "$body" in
        *'"method":"initialize"'*)
          send '{"jsonrpc":"2.0","id":1,"result":{}}'
          ;;
      esac
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

/// Compose the fixtures with one extra plugin and the process environment.
pub fn compose_with(plugin: Box<dyn Plugin>) -> Result<Runtime, ComposeError> {
    compose_with_env(plugin, &SystemEnv)
}

/// Compose the fixtures with one extra plugin and an injected environment.
pub fn compose_with_env(plugin: Box<dyn Plugin>, env: &dyn Env) -> Result<Runtime, ComposeError> {
    let plugins: Vec<Box<dyn Plugin>> =
        vec![Box::new(StorePlugin), Box::new(ProviderPlugin), plugin];
    compose(&plugins, &Config::default(), env)
}
