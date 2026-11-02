//! Blocking LSP stdio transport and the connection seam tools share.

use crate::config::LspServerConfig;
use crate::error::LspError;
use crate::format::path_to_uri;
use crate::framing::{encode_frame, read_frame, response_result};
use serde_json::{Value, json};
use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// One live language server that the tool calls through.
///
/// Implementations are shared behind an `Arc`, so calls may arrive from several
/// tools at once and must serialize whatever the underlying transport cannot.
pub trait LspConnection: Send + Sync {
    /// Send `textDocument/didOpen` for one file.
    fn open(&self, path: &str, language_id: &str, text: &str) -> Result<(), LspError>;

    /// Send one request and return the result of its matching response.
    fn request(&self, method: &str, params: Value) -> Result<Value, LspError>;
}

/// A language server reached over framed JSON-RPC on piped stdio.
pub struct ProcessConnection {
    state: Mutex<ProcessState>,
}

struct ProcessState {
    server: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

/// A peer that only ever sends notifications is broken, not merely chatty.
const MAX_UNMATCHED_FRAMES: usize = 1024;

impl ProcessConnection {
    /// Start `config` and complete the initialize handshake.
    pub fn spawn(config: &LspServerConfig) -> Result<Self, LspError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|error| LspError::Server {
            server: config.name.clone(),
            message: format!("failed to start {}: {error}", config.command),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| LspError::Server {
            server: config.name.clone(),
            message: "child stdin was not piped".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| LspError::Server {
            server: config.name.clone(),
            message: "child stdout was not piped".to_string(),
        })?;
        let root = std::env::current_dir().unwrap_or_default();
        let mut state = ProcessState {
            server: config.name.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        state.initialize(&root)?;
        Ok(Self {
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, ProcessState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LspConnection for ProcessConnection {
    fn open(&self, path: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        let mut state = self.lock();
        state.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": path_to_uri(Path::new(path)),
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }
        }))
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let mut state = self.lock();
        let id = state.take_id();
        state.request(method, &params, id)
    }
}

impl Drop for ProcessConnection {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        let _ = state.child.kill();
        let _ = state.child.wait();
    }
}

impl ProcessState {
    fn initialize(&mut self, root: &Path) -> Result<(), LspError> {
        let id = self.take_id();
        let params = json!({
            "processId": std::process::id(),
            "rootUri": path_to_uri(root),
            "capabilities": {},
            "clientInfo": {
                "name": "deepseacrab",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        self.request("initialize", &params, id)?;
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
    }

    const fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn request(&mut self, method: &str, params: &Value, id: u64) -> Result<Value, LspError> {
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&message)?;
        for _ in 0..MAX_UNMATCHED_FRAMES {
            let frame = read_frame(&mut self.stdout)?;
            if let Some(result) = response_result(&frame, id) {
                return result;
            }
        }
        Err(LspError::Protocol(format!(
            "server sent {MAX_UNMATCHED_FRAMES} frames without answering request {id}"
        )))
    }

    fn send(&mut self, message: &Value) -> Result<(), LspError> {
        let frame = encode_frame(message)?;
        self.stdin
            .write_all(&frame)
            .and_then(|()| self.stdin.flush())
            .map_err(|error| self.io_error("write to server", &error))
    }

    fn io_error(&self, action: &str, error: &std::io::Error) -> LspError {
        LspError::Server {
            server: self.server.clone(),
            message: format!("{action}: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{lingering_server, noisy_server, pid_path, run_blocking, stub_server};
    use serde_json::json;

    fn config(name: &str, command: &str, args: Vec<&str>) -> LspServerConfig {
        LspServerConfig {
            name: name.to_string(),
            command: command.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            extensions: vec!["rs".to_string()],
        }
    }

    #[test]
    fn a_missing_executable_names_the_server() {
        let error =
            ProcessConnection::spawn(&config("broken", "/nonexistent/dsh-lsp-server", Vec::new()))
                .err()
                .expect("spawn fails");
        match error {
            LspError::Server { server, message } => {
                assert_eq!(server, "broken");
                assert!(message.contains("failed to start"));
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }
    }

    #[test]
    fn a_server_that_exits_is_reported() {
        let config = config("closing", "/bin/sh", vec!["-c", "sleep 0.2"]);
        let error = run_blocking(move || ProcessConnection::spawn(&config).err())
            .expect("expected a spawn failure");
        assert_eq!(
            error,
            LspError::Protocol("server closed stdout before answering".to_string())
        );
    }

    #[test]
    fn stdio_round_trips_open_and_hover() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = stub_server(directory.path(), "stub");
        let path = directory.path().join("sample.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");
        let uri = path_to_uri(&path);

        let result = run_blocking(move || {
            let connection = ProcessConnection::spawn(&config)?;
            connection.open(path.to_string_lossy().as_ref(), "rust", "fn main() {}\n")?;
            connection.request(
                "textDocument/hover",
                json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": 0, "character": 0},
                }),
            )
        })
        .expect("hover");
        assert_eq!(
            result,
            json!({"contents": {"kind": "markdown", "value": "stub hover"}})
        );
    }

    #[test]
    fn a_flood_of_notifications_without_an_answer_is_reported() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = noisy_server(directory.path(), "noisy");
        let error = run_blocking(move || {
            let connection = ProcessConnection::spawn(&config)?;
            connection.request("textDocument/hover", json!({}))
        })
        .expect_err("request fails");
        assert_eq!(
            error,
            LspError::Protocol("server sent 1024 frames without answering request 2".to_string())
        );
    }

    #[test]
    fn dropping_the_connection_kills_the_server() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = lingering_server(directory.path());
        let connection = run_blocking(move || ProcessConnection::spawn(&config)).expect("spawn");
        let pid_path = pid_path(directory.path());
        drop(connection);

        let pid = std::fs::read_to_string(&pid_path).expect("pid file");
        let status = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("kill");
        assert!(!status.success(), "server {pid} survived the drop");
    }
}
