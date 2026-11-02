//! JSON-RPC framing and result decoding for the MCP stdio transport.
//!
//! Everything here is a pure transform over values and lines, so the protocol
//! rules can be tested without a child process.

use crate::error::McpError;
use serde_json::{Value, json};

/// Protocol revision this client speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Client name reported during initialization.
pub const CLIENT_NAME: &str = "deepseacrab-harness";

/// One tool declared by a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolSpec {
    /// Tool name as the server knows it.
    pub name: String,
    /// Model-facing description.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub input_schema: Value,
}

/// The normalized result of one remote tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolResult {
    /// Concatenated text content.
    pub text: String,
    /// Whether the server marked the call failed.
    pub is_error: bool,
}

/// Build the `initialize` request.
pub fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

/// Build the `notifications/initialized` notification.
pub fn initialized_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })
}

/// Build the `tools/list` request.
pub fn tools_list_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list"
    })
}

/// Build the `tools/call` request.
pub fn tool_call_request(id: u64, name: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments
        }
    })
}

/// Decode one line, returning the result only for the expected request id.
///
/// Notifications carry no id and responses to other requests carry a different
/// one; both are skipped so interleaved server traffic cannot desynchronize the
/// client.
pub fn response_result(line: &str, expected_id: u64) -> Result<Option<Value>, McpError> {
    let message: Value = serde_json::from_str(line)
        .map_err(|error| McpError::Protocol(format!("invalid JSON-RPC frame: {error}")))?;
    if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Ok(None);
    }
    if let Some(error) = message.get("error") {
        return Err(decode_rpc_error(error));
    }
    message.get("result").map_or_else(
        || {
            Err(McpError::Protocol(
                "JSON-RPC response has no result".to_string(),
            ))
        },
        |result| Ok(Some(result.clone())),
    )
}

fn decode_rpc_error(error: &Value) -> McpError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    error.get("code").and_then(Value::as_i64).map_or_else(
        || McpError::Rpc(message.to_string()),
        |code| McpError::Rpc(format!("{message} (code {code})")),
    )
}

/// Decode the `tools/list` result.
pub fn parse_tools(result: &Value) -> Result<Vec<McpToolSpec>, McpError> {
    let tools = result
        .get("tools")
        .ok_or_else(|| McpError::Protocol("tools/list result has no tools".to_string()))?;
    let tools = tools
        .as_array()
        .ok_or_else(|| McpError::Protocol("tools/list tools is not an array".to_string()))?;
    tools.iter().map(parse_tool_spec).collect()
}

fn parse_tool_spec(value: &Value) -> Result<McpToolSpec, McpError> {
    let object = value
        .as_object()
        .ok_or_else(|| McpError::Protocol("tools/list entry is not an object".to_string()))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| McpError::Protocol("tools/list entry has no non-empty name".to_string()))?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_schema = object
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    Ok(McpToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    })
}

/// Decode the `tools/call` result.
pub fn parse_tool_result(result: &Value) -> Result<McpToolResult, McpError> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Protocol("tools/call result has no content array".to_string()))?;
    let mut text = String::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(part) = block.get("text").and_then(Value::as_str) {
                text.push_str(part);
            }
        }
    }
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(McpToolResult { text, is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_request_carries_the_protocol_contract() {
        assert_eq!(
            initialize_request(7),
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "deepseacrab-harness",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            })
        );
    }

    #[test]
    fn initialized_notification_has_no_id() {
        assert_eq!(
            initialized_notification(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        );
    }

    #[test]
    fn tools_requests_name_the_operation() {
        assert_eq!(
            tools_list_request(2),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        );
        assert_eq!(
            tool_call_request(3, "echo", &json!({"text": "hi"})),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "echo", "arguments": {"text": "hi"}}
            })
        );
    }

    #[test]
    fn a_matching_result_is_returned() {
        let line = r#"{"jsonrpc":"2.0","id":4,"result":{"tools":[]}}"#;
        assert_eq!(
            response_result(line, 4).expect("decodes"),
            Some(json!({"tools": []}))
        );
    }

    #[test]
    fn notifications_and_other_ids_are_skipped() {
        assert_eq!(
            response_result(r#"{"jsonrpc":"2.0","method":"notifications/message"}"#, 4)
                .expect("decodes"),
            None
        );
        assert_eq!(
            response_result(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#, 4).expect("decodes"),
            None
        );
    }

    #[test]
    fn rpc_errors_become_client_errors() {
        assert_eq!(
            response_result(
                r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"no such method"}}"#,
                4
            ),
            Err(McpError::Rpc("no such method (code -32601)".to_string()))
        );
        assert_eq!(
            response_result(r#"{"jsonrpc":"2.0","id":4,"error":{"message":"plain"}}"#, 4),
            Err(McpError::Rpc("plain".to_string()))
        );
        assert_eq!(
            response_result(r#"{"jsonrpc":"2.0","id":4,"error":{}}"#, 4),
            Err(McpError::Rpc("unknown error".to_string()))
        );
    }

    #[test]
    fn malformed_frames_are_rejected() {
        assert!(matches!(
            response_result("not json", 4),
            Err(McpError::Protocol(_))
        ));
        assert_eq!(
            response_result(r#"{"jsonrpc":"2.0","id":4}"#, 4),
            Err(McpError::Protocol(
                "JSON-RPC response has no result".to_string()
            ))
        );
    }

    #[test]
    fn tools_decode_with_defaults() {
        let result = json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "Echo text",
                    "inputSchema": {"type": "object", "properties": {}}
                },
                {"name": "bare"}
            ]
        });
        assert_eq!(
            parse_tools(&result).expect("decodes"),
            vec![
                McpToolSpec {
                    name: "echo".to_string(),
                    description: "Echo text".to_string(),
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                McpToolSpec {
                    name: "bare".to_string(),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                },
            ]
        );
    }

    #[test]
    fn malformed_tool_lists_are_rejected() {
        for result in [
            json!({}),
            json!({"tools": "nope"}),
            json!({"tools": [7]}),
            json!({"tools": [{}]}),
            json!({"tools": [{"name": ""}]}),
            json!({"tools": [{"name": 3}]}),
        ] {
            assert!(matches!(parse_tools(&result), Err(McpError::Protocol(_))));
        }
    }

    #[test]
    fn text_blocks_are_joined_and_non_text_blocks_are_ignored() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "..."},
                {"type": "text"},
                {"type": "text", "text": " world"}
            ]
        });
        assert_eq!(
            parse_tool_result(&result).expect("decodes"),
            McpToolResult {
                text: "hello world".to_string(),
                is_error: false,
            }
        );
    }

    #[test]
    fn the_error_flag_is_preserved() {
        let result = json!({"content": [{"type": "text", "text": "boom"}], "isError": true});
        assert_eq!(
            parse_tool_result(&result).expect("decodes"),
            McpToolResult {
                text: "boom".to_string(),
                is_error: true,
            }
        );
        let ignored = json!({"content": [], "isError": "yes"});
        assert_eq!(
            parse_tool_result(&ignored).expect("decodes"),
            McpToolResult {
                text: String::new(),
                is_error: false,
            }
        );
    }

    #[test]
    fn results_without_content_are_rejected() {
        assert!(matches!(
            parse_tool_result(&json!({})),
            Err(McpError::Protocol(_))
        ));
        assert!(matches!(
            parse_tool_result(&json!({"content": {}})),
            Err(McpError::Protocol(_))
        ));
    }
}
