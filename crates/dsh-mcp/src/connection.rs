//! Blocking MCP stdio transport and the connection seam tools share.

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::protocol::{self, McpToolResult, McpToolSpec};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// One live MCP server that tools call through.
///
/// Implementations are shared behind an `Arc`, so calls may arrive from several
/// tools at once and must serialize whatever the underlying transport cannot.
pub trait McpConnection: Send + Sync {
    /// Ask the server for its tool declarations.
    fn list_tools(&self) -> Result<Vec<McpToolSpec>, McpError>;

    /// Invoke one tool with a JSON arguments object.
    fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult, McpError>;
}

/// An MCP server reached over piped JSON-RPC newline-delimited stdio.
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

impl ProcessConnection {
    /// Start `config` and complete the initialize handshake.
    pub fn spawn(config: &McpServerConfig) -> Result<Self, McpError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in &config.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|error| McpError::Server {
            server: config.name.clone(),
            message: format!("failed to start {}: {error}", config.command),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Server {
            server: config.name.clone(),
            message: "child stdin was not piped".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Server {
            server: config.name.clone(),
            message: "child stdout was not piped".to_string(),
        })?;
        let mut state = ProcessState {
            server: config.name.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        state.initialize()?;
        Ok(Self {
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, ProcessState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl McpConnection for ProcessConnection {
    fn list_tools(&self) -> Result<Vec<McpToolSpec>, McpError> {
        let mut state = self.lock();
        let id = state.take_id();
        let result = state.request(&protocol::tools_list_request(id), id)?;
        drop(state);
        protocol::parse_tools(&result)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult, McpError> {
        let mut state = self.lock();
        let id = state.take_id();
        let request = protocol::tool_call_request(id, name, &arguments);
        let result = state.request(&request, id)?;
        drop(state);
        protocol::parse_tool_result(&result)
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
    fn initialize(&mut self) -> Result<(), McpError> {
        let id = self.take_id();
        let request = protocol::initialize_request(id);
        self.request(&request, id)?;
        self.send(&protocol::initialized_notification())
    }

    const fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn request(&mut self, request: &Value, id: u64) -> Result<Value, McpError> {
        self.send(request)?;
        self.await_result(id)
    }

    fn send(&mut self, message: &Value) -> Result<(), McpError> {
        let line = serde_json::to_string(message)
            .map_err(|error| McpError::Protocol(format!("could not encode frame: {error}")))?;
        let written = writeln!(self.stdin, "{line}").and_then(|()| self.stdin.flush());
        written.map_err(|error| self.io_error("write to server", &error))
    }

    fn await_result(&mut self, id: u64) -> Result<Value, McpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self.stdout.read_line(&mut line);
            let read = read.map_err(|error| self.io_error("read from server", &error))?;
            if read == 0 {
                return Err(McpError::Protocol(
                    "server closed stdout before answering".to_string(),
                ));
            }
            if let Some(result) = protocol::response_result(line.trim_end(), id)? {
                return Ok(result);
            }
        }
    }

    fn io_error(&self, action: &str, error: &std::io::Error) -> McpError {
        McpError::Server {
            server: self.server.clone(),
            message: format!("{action}: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;
    use serde_json::json;

    fn expected_spec() -> McpToolSpec {
        McpToolSpec {
            name: "echo".to_string(),
            description: "Echo text".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}}
            }),
        }
    }

    #[test]
    fn a_missing_executable_names_the_server() {
        let config = McpServerConfig {
            name: "broken".to_string(),
            command: "/nonexistent/dsh-mcp-server".to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let error = ProcessConnection::spawn(&config)
            .err()
            .expect("spawn fails");
        match error {
            McpError::Server { server, message } => {
                assert_eq!(server, "broken");
                assert!(message.contains("failed to start"));
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }
    }

    #[test]
    fn a_server_that_exits_is_reported() {
        let config = McpServerConfig {
            name: "closing".to_string(),
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "sleep 0.2".to_string()],
            env: Vec::new(),
        };
        assert_eq!(
            ProcessConnection::spawn(&config).err(),
            Some(McpError::Protocol(
                "server closed stdout before answering".to_string()
            ))
        );
    }

    #[test]
    fn stdio_round_trips_tools_and_calls() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = test_util::stub_server(directory.path(), "stub");
        let connection = ProcessConnection::spawn(&config).expect("spawn");

        assert_eq!(
            connection.list_tools().expect("list"),
            vec![expected_spec()]
        );
        assert_eq!(
            connection
                .call_tool("echo", json!({"text": "hi"}))
                .expect("call"),
            McpToolResult {
                text: "stub result".to_string(),
                is_error: false,
            }
        );
        assert_eq!(
            connection
                .call_tool("echo", json!({"text": "again"}))
                .expect("call"),
            McpToolResult {
                text: "stub result".to_string(),
                is_error: false,
            }
        );
    }

    #[test]
    fn configured_environment_reaches_the_server() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut config = test_util::stub_server(directory.path(), "stub");
        config
            .env
            .push(("STUB_TEXT".to_string(), "from env ".to_string()));
        let connection = ProcessConnection::spawn(&config).expect("spawn");
        connection.list_tools().expect("list");
        assert_eq!(
            connection
                .call_tool("echo", json!({"text": "hi"}))
                .expect("call")
                .text,
            "from env result"
        );
    }

    #[test]
    fn dropping_the_connection_kills_the_server() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = test_util::lingering_server(directory.path());
        let pid_path = test_util::pid_path(directory.path());
        let connection = ProcessConnection::spawn(&config).expect("spawn");
        drop(connection);

        let pid = std::fs::read_to_string(&pid_path).expect("pid file");
        let status = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("kill");
        assert!(!status.success(), "server {pid} survived the drop");
    }
}
