//! A `dsh_core::Tool` backed by one remote MCP tool.

use crate::connection::McpConnection;
use crate::protocol::McpToolSpec;
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde_json::Value;
use std::sync::Arc;

/// One external MCP tool exposed under a server-namespaced name.
pub struct McpTool {
    server: String,
    spec: McpToolSpec,
    connection: Arc<dyn McpConnection>,
}

impl McpTool {
    /// Bind one server tool to the connection that serves it.
    #[must_use]
    pub fn new(
        server: impl Into<String>,
        spec: McpToolSpec,
        connection: Arc<dyn McpConnection>,
    ) -> Self {
        Self {
            server: server.into(),
            spec,
            connection,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: format!("mcp_{}_{}", self.server, self.spec.name),
            description: self.spec.description.clone(),
            parameters: self.spec.input_schema.clone(),
        }
    }

    async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
        let connection = Arc::clone(&self.connection);
        let name = self.spec.name.clone();
        let outcome =
            tokio::task::spawn_blocking(move || connection.call_tool(&name, arguments)).await;
        match outcome {
            Ok(Ok(result)) if result.is_error => ToolOutcome::error(result.text),
            Ok(Ok(result)) => ToolOutcome::text(result.text),
            Ok(Err(error)) => {
                ToolOutcome::error(format!("mcp tool {} failed: {error}", self.spec.name))
            }
            Err(error) => {
                ToolOutcome::error(format!("mcp tool {} task failed: {error}", self.spec.name))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockConnection, MockMode, spec, tool_context};
    use dsh_core::Tool;
    use serde_json::json;
    use std::sync::Arc;

    fn tool(mode: MockMode, tool_name: &str) -> (McpTool, Arc<MockConnection>) {
        let connection = Arc::new(MockConnection::new(vec![spec(tool_name)], mode));
        let tool = McpTool::new(
            "srv",
            spec(tool_name),
            Arc::clone(&connection) as Arc<dyn McpConnection>,
        );
        (tool, connection)
    }

    #[test]
    fn the_schema_namespaces_the_server_and_keeps_the_declaration() {
        let (tool, _) = tool(MockMode::Ok, "echo");
        let schema = tool.schema();
        assert_eq!(schema.name, "mcp_srv_echo");
        assert_eq!(schema.description, "echo tool");
        assert_eq!(
            schema.parameters,
            json!({"type": "object", "properties": {}})
        );
    }

    #[tokio::test]
    async fn execute_forwards_the_original_name_and_arguments() {
        let (tool, connection) = tool(MockMode::Ok, "echo");
        let outcome = tool.run(&tool_context(), r#"{"text":"hi"}"#).await;
        assert_eq!(outcome.text_content(), "echo result");
        assert!(!outcome.is_error);
        assert_eq!(
            connection.calls(),
            vec![("echo".to_string(), json!({"text": "hi"}))]
        );
    }

    #[tokio::test]
    async fn a_server_error_flag_becomes_an_error_outcome() {
        let (tool, _) = tool(MockMode::Ok, "bad");
        let outcome = tool.run(&tool_context(), "{}").await;
        assert_eq!(outcome.text_content(), "bad result");
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn a_connection_failure_becomes_an_error_outcome() {
        let (tool, _) = tool(MockMode::CallFails, "echo");
        let outcome = tool.run(&tool_context(), "{}").await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("mcp tool echo failed"));
    }

    #[tokio::test]
    async fn a_panicking_call_is_contained_at_the_task_boundary() {
        let (tool, _) = tool(MockMode::Panics, "echo");
        let outcome = tool.run(&tool_context(), "{}").await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("mcp tool echo task failed"));
    }
}
