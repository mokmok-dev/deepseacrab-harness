//! The single `lsp` tool, routed to one connection per configured server.

use crate::config::LspServerConfig;
use crate::connection::LspConnection;
use crate::format::{self, cap_output, language_id};
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::sync::Arc;

/// One configured server bound to its live connection.
pub struct LspServer {
    /// Configuration the connection was started from.
    pub config: LspServerConfig,
    /// Live transport the tool queries.
    pub connection: Arc<dyn LspConnection>,
}

impl LspServer {
    /// Bind a configuration to the connection that serves it.
    #[must_use]
    pub fn new(config: LspServerConfig, connection: Arc<dyn LspConnection>) -> Self {
        Self { config, connection }
    }
}

/// The model-facing tool that answers one query at a time.
pub struct LspTool {
    servers: Vec<LspServer>,
}

impl LspTool {
    /// Build a tool over already-connected servers.
    #[must_use]
    pub const fn new(servers: Vec<LspServer>) -> Self {
        Self { servers }
    }

    fn server_for(&self, extension: &str) -> Option<&LspServer> {
        self.servers.iter().find(|server| {
            server
                .config
                .extensions
                .iter()
                .any(|candidate| candidate == extension)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Hover,
    Definition,
    References,
    Symbols,
}

impl Operation {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "hover" => Some(Self::Hover),
            "definition" => Some(Self::Definition),
            "references" => Some(Self::References),
            "symbols" => Some(Self::Symbols),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::References => "references",
            Self::Symbols => "symbols",
        }
    }

    const fn method(self) -> &'static str {
        match self {
            Self::Hover => "textDocument/hover",
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Symbols => "textDocument/documentSymbol",
        }
    }

    const fn needs_position(self) -> bool {
        !matches!(self, Self::Symbols)
    }
}

fn render(operation: Operation, result: &Value) -> String {
    match operation {
        Operation::Hover => format::hover_text(result),
        Operation::Definition | Operation::References => format::locations_text(result),
        Operation::Symbols => format::symbols_text(result),
    }
}

#[async_trait]
impl Tool for LspTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "lsp".to_string(),
            description: "Query the language server configured for a file: hover, definition, \
                          references, or document symbols. Lines and characters are 1-based."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["hover", "definition", "references", "symbols"],
                        "description": "Query to run."
                    },
                    "path": {
                        "type": "string",
                        "description": "File path, resolved against the session working directory."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based line number, required for every operation except symbols."
                    },
                    "character": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based character offset, required for every operation except symbols."
                    }
                },
                "required": ["operation", "path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let Some(operation) = arguments
            .get("operation")
            .and_then(Value::as_str)
            .and_then(Operation::parse)
        else {
            return ToolOutcome::error(
                "unsupported lsp operation: expected hover, definition, references, or symbols",
            );
        };
        let Some(raw_path) = arguments.get("path").and_then(Value::as_str) else {
            return ToolOutcome::error("lsp tool requires a string path");
        };
        let path = match context.readable_path(raw_path) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let Some(extension) = path.extension().and_then(OsStr::to_str) else {
            return ToolOutcome::error(format!("no lsp server configured for {raw_path}"));
        };
        let Some(server) = self.server_for(extension) else {
            return ToolOutcome::error(format!("no lsp server configured for {raw_path}"));
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                return ToolOutcome::error(format!("could not read {}: {error}", path.display()));
            }
        };
        let uri = format::path_to_uri(&path);
        let params = if operation.needs_position() {
            let position = arguments
                .get("line")
                .and_then(Value::as_u64)
                .zip(arguments.get("character").and_then(Value::as_u64))
                .and_then(|(line, character)| format::position(&text, line, character));
            let Some(position) = position else {
                return ToolOutcome::error(format!(
                    "lsp {} requires a 1-based line and character inside {raw_path}",
                    operation.as_str()
                ));
            };
            let mut params = json!({"textDocument": {"uri": uri}, "position": position});
            if operation == Operation::References {
                params["context"] = json!({"includeDeclaration": true});
            }
            params
        } else {
            json!({"textDocument": {"uri": uri}})
        };

        let connection = Arc::clone(&server.connection);
        let absolute = path.to_string_lossy().into_owned();
        let language_id = language_id(extension).to_string();
        let method = operation.method();
        let outcome = tokio::task::spawn_blocking(move || {
            connection.open(&absolute, &language_id, &text)?;
            connection.request(method, params)
        })
        .await;
        match outcome {
            Ok(Ok(result)) => ToolOutcome::text(cap_output(render(operation, &result))),
            Ok(Err(error)) => {
                ToolOutcome::error(format!("lsp {} failed: {error}", operation.as_str()))
            }
            Err(error) => {
                ToolOutcome::error(format!("lsp {} task failed: {error}", operation.as_str()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockConnection, MockMode};
    use dsh_core::PermissionMode;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    const SAMPLE: &str = "fn main() {}\n";

    fn config(name: &str, extensions: &[&str]) -> LspServerConfig {
        LspServerConfig {
            name: name.to_string(),
            command: "mock".to_string(),
            args: Vec::new(),
            extensions: extensions.iter().map(ToString::to_string).collect(),
        }
    }

    fn connected(config: LspServerConfig, connection: &Arc<MockConnection>) -> LspServer {
        let connection: Arc<dyn LspConnection> = Arc::clone(connection) as Arc<dyn LspConnection>;
        LspServer::new(config, connection)
    }

    fn server(name: &str, extensions: &[&str]) -> LspServer {
        LspServer::new(
            config(name, extensions),
            Arc::new(MockConnection::new(MockMode::Ok)),
        )
    }

    fn context_for(directory: &Path) -> ToolContext {
        ToolContext::new(
            directory.to_path_buf(),
            PermissionMode::ReadOnly,
            CancellationToken::new(),
        )
    }

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("sample.rs"), SAMPLE).expect("write");
        let context = context_for(directory.path());
        (directory, context)
    }

    #[test]
    fn the_schema_declares_the_four_operations() {
        let tool = LspTool::new(Vec::new());
        let schema = tool.schema();
        assert_eq!(schema.name, "lsp");
        assert_eq!(
            schema.parameters["properties"]["operation"]["enum"],
            json!(["hover", "definition", "references", "symbols"])
        );
        assert_eq!(schema.parameters["required"], json!(["operation", "path"]));
        assert!(schema.description.contains("1-based"));
    }

    #[tokio::test]
    async fn hover_sends_open_and_a_zero_based_position() {
        let (directory, context) = workspace();
        let mock = Arc::new(MockConnection::new(MockMode::Ok));
        let tool = LspTool::new(vec![connected(config("rust", &["rs"]), &mock)]);

        let outcome = tool
            .run(
                &context,
                r#"{"operation":"hover","path":"sample.rs","line":1,"character":2}"#,
            )
            .await;
        assert_eq!(outcome.text_content(), "mock hover");
        assert!(!outcome.is_error);

        let opens = mock.opens();
        assert_eq!(opens.len(), 1);
        assert_eq!(
            opens[0].0,
            directory.path().join("sample.rs").to_string_lossy()
        );
        assert_eq!(opens[0].1, "rust");
        assert_eq!(opens[0].2, SAMPLE);
        assert_eq!(
            mock.requests(),
            vec![(
                "textDocument/hover".to_string(),
                json!({
                    "textDocument": {
                        "uri": format::path_to_uri(&directory.path().join("sample.rs"))
                    },
                    "position": {"line": 0, "character": 1},
                })
            )]
        );
    }

    #[tokio::test]
    async fn definition_formats_locations() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"definition","path":"sample.rs","line":1,"character":1}"#,
            )
            .await;
        assert_eq!(outcome.text_content(), "/tmp/mock.rs:3:5");
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn references_ask_for_declarations() {
        let (directory, context) = workspace();
        let mock = Arc::new(MockConnection::new(MockMode::Ok));
        let tool = LspTool::new(vec![connected(config("rust", &["rs"]), &mock)]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"references","path":"sample.rs","line":1,"character":1}"#,
            )
            .await;
        assert_eq!(outcome.text_content(), "/tmp/mock.rs:3:5\n/tmp/mock.rs:9:1");
        assert_eq!(
            mock.requests(),
            vec![(
                "textDocument/references".to_string(),
                json!({
                    "textDocument": {
                        "uri": format::path_to_uri(&directory.path().join("sample.rs"))
                    },
                    "position": {"line": 0, "character": 0},
                    "context": {"includeDeclaration": true},
                })
            )]
        );
    }

    #[tokio::test]
    async fn symbols_need_no_position() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        let outcome = tool
            .run(&context, r#"{"operation":"symbols","path":"sample.rs"}"#)
            .await;
        assert_eq!(
            outcome.text_content(),
            "main (Function)\n  local (Variable)"
        );
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn the_first_server_claiming_an_extension_wins() {
        let (_directory, context) = workspace();
        let first = Arc::new(MockConnection::new(MockMode::Ok));
        let second = Arc::new(MockConnection::new(MockMode::Ok));
        let tool = LspTool::new(vec![
            connected(config("first", &["rs", "py"]), &first),
            connected(config("second", &["rs"]), &second),
        ]);
        tool.run(&context, r#"{"operation":"symbols","path":"sample.rs"}"#)
            .await;
        assert_eq!(first.requests().len(), 1);
        assert_eq!(second.requests(), Vec::new());
    }

    #[tokio::test]
    async fn an_extension_owned_by_a_later_server_is_found() {
        let (directory, context) = workspace();
        std::fs::write(directory.path().join("sample.py"), "x = 1\n").expect("write");
        let rust = Arc::new(MockConnection::new(MockMode::Ok));
        let python = Arc::new(MockConnection::new(MockMode::Ok));
        let tool = LspTool::new(vec![
            connected(config("rust", &["rs"]), &rust),
            connected(config("python", &["py"]), &python),
        ]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"hover","path":"sample.py","line":1,"character":1}"#,
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(rust.requests(), Vec::new());
        assert_eq!(python.requests().len(), 1);
        assert_eq!(python.opens()[0].1, "python");
    }

    #[tokio::test]
    async fn an_unconfigured_extension_is_an_error() {
        let (directory, context) = workspace();
        std::fs::write(directory.path().join("sample.txt"), "text\n").expect("write");
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        for path in ["sample.txt", "extensionless"] {
            let outcome = tool
                .run(
                    &context,
                    &format!(r#"{{"operation":"symbols","path":"{path}"}}"#),
                )
                .await;
            assert!(outcome.is_error);
            assert_eq!(
                outcome.text_content(),
                format!("no lsp server configured for {path}")
            );
        }
    }

    #[tokio::test]
    async fn an_unsupported_operation_is_an_error() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        for arguments in [
            r#"{"operation":"rename","path":"sample.rs"}"#,
            r#"{"path":"sample.rs"}"#,
            r#"{"operation":7,"path":"sample.rs"}"#,
        ] {
            let outcome = tool.run(&context, arguments).await;
            assert!(outcome.is_error);
            assert_eq!(
                outcome.text_content(),
                "unsupported lsp operation: expected hover, definition, references, or symbols"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_path_is_an_error() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        let outcome = tool.run(&context, r#"{"operation":"hover"}"#).await;
        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), "lsp tool requires a string path");
    }

    #[tokio::test]
    async fn an_unreadable_file_is_an_error() {
        let (directory, context) = workspace();
        std::fs::create_dir(directory.path().join("directory.rs")).expect("mkdir");
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        let outcome = tool
            .run(&context, r#"{"operation":"symbols","path":"directory.rs"}"#)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("could not read"));
        assert!(outcome.text_content().contains("directory.rs"));
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_refused() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![server("rust", &["rs"])]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"symbols","path":"/etc/passwd.rs"}"#,
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("/etc/passwd.rs"));
    }

    #[tokio::test]
    async fn bad_positions_are_rejected_before_any_request() {
        let (_directory, context) = workspace();
        let mock = Arc::new(MockConnection::new(MockMode::Ok));
        let tool = LspTool::new(vec![connected(config("rust", &["rs"]), &mock)]);
        for (line, character) in [(0, 1), (1, 0), (3, 1), (1, 20)] {
            let outcome = tool
                .run(
                    &context,
                    &format!(
                        r#"{{"operation":"hover","path":"sample.rs","line":{line},"character":{character}}}"#
                    ),
                )
                .await;
            assert!(outcome.is_error, "accepted {line}:{character}");
            assert_eq!(
                outcome.text_content(),
                "lsp hover requires a 1-based line and character inside sample.rs"
            );
        }
        for arguments in [
            r#"{"operation":"hover","path":"sample.rs","line":1}"#,
            r#"{"operation":"hover","path":"sample.rs","character":1}"#,
            r#"{"operation":"hover","path":"sample.rs","line":"1","character":1}"#,
        ] {
            let outcome = tool.run(&context, arguments).await;
            assert!(outcome.is_error);
            assert_eq!(
                outcome.text_content(),
                "lsp hover requires a 1-based line and character inside sample.rs"
            );
        }
        assert_eq!(mock.requests(), Vec::new());
        assert_eq!(mock.opens(), Vec::new());
    }

    #[tokio::test]
    async fn the_last_position_on_the_trailing_line_is_accepted() {
        let (_directory, context) = workspace();
        let mock = Arc::new(MockConnection::new(MockMode::Ok));
        let tool = LspTool::new(vec![connected(config("rust", &["rs"]), &mock)]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"hover","path":"sample.rs","line":2,"character":1}"#,
            )
            .await;
        assert!(!outcome.is_error);
        assert_eq!(
            mock.requests()[0].1["position"],
            json!({"line": 1, "character": 0})
        );
    }

    #[tokio::test]
    async fn a_connection_failure_becomes_an_error_outcome() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![LspServer::new(
            config("rust", &["rs"]),
            Arc::new(MockConnection::new(MockMode::RequestFails)),
        )]);
        let outcome = tool
            .run(&context, r#"{"operation":"symbols","path":"sample.rs"}"#)
            .await;
        assert!(outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "lsp symbols failed: lsp protocol error: mock request failed"
        );
    }

    #[tokio::test]
    async fn an_open_failure_stops_before_the_request() {
        let (_directory, context) = workspace();
        let mock = Arc::new(MockConnection::new(MockMode::OpenFails));
        let tool = LspTool::new(vec![connected(config("rust", &["rs"]), &mock)]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"hover","path":"sample.rs","line":1,"character":1}"#,
            )
            .await;
        assert!(outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "lsp hover failed: lsp protocol error: mock open failed"
        );
        assert_eq!(mock.requests(), Vec::new());
    }

    #[tokio::test]
    async fn a_panicking_connection_is_contained_at_the_task_boundary() {
        let (_directory, context) = workspace();
        let tool = LspTool::new(vec![LspServer::new(
            config("rust", &["rs"]),
            Arc::new(MockConnection::new(MockMode::Panics)),
        )]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"hover","path":"sample.rs","line":1,"character":1}"#,
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("lsp hover task failed"));
    }

    #[tokio::test]
    async fn large_results_are_capped() {
        let (_directory, context) = workspace();
        let huge = "x".repeat(format::MAX_OUTPUT_BYTES * 2);
        let mock = Arc::new(MockConnection::with_hover(json!({"contents": huge})));
        let tool = LspTool::new(vec![connected(config("rust", &["rs"]), &mock)]);
        let outcome = tool
            .run(
                &context,
                r#"{"operation":"hover","path":"sample.rs","line":1,"character":1}"#,
            )
            .await;
        let text = outcome.text_content();
        assert_eq!(text.len(), format::MAX_OUTPUT_BYTES);
        assert!(text.ends_with("[truncated]"));
    }
}
