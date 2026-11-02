//! JSON-RPC 2.0 frames for the line-delimited SDK transport.
//!
//! Decoding and frame construction are pure transforms over one line, so the
//! wire rules can be tested without an IO transport.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The line was not JSON.
pub const PARSE_ERROR: i32 = -32700;

/// The request shape or its parameters did not match the method.
pub const INVALID_PARAMS: i32 = -32602;

/// No method has this name.
pub const METHOD_NOT_FOUND: i32 = -32601;

/// The harness failed while handling a well-formed request.
pub const INTERNAL_ERROR: i32 = -32603;

/// The `id` a request carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A JSON number.
    Number(serde_json::Number),
    /// A JSON string.
    String(String),
}

/// One decoded request frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Caller-chosen correlation id, absent when the frame omitted it.
    pub id: Option<RequestId>,
    /// Method name.
    pub method: String,
    /// Method parameters, [`Value::Null`] when the frame omitted them.
    pub params: Value,
}

/// Why a line could not become a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The line was not JSON at all.
    Syntax(String),
    /// The line parsed as JSON but not as a request.
    Shape {
        /// The id to echo, when the frame carried a representable one.
        id: Option<RequestId>,
        /// Human-readable detail.
        message: String,
    },
}

/// Decode one line into a request.
pub fn decode(line: &str) -> Result<Request, DecodeError> {
    let frame: Value =
        serde_json::from_str(line).map_err(|error| DecodeError::Syntax(error.to_string()))?;
    let id = frame.get("id").and_then(parse_id);
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        return Err(DecodeError::Shape {
            id,
            message: "request must carry a string method".to_string(),
        });
    };
    Ok(Request {
        id,
        method: method.to_string(),
        params: frame.get("params").cloned().unwrap_or(Value::Null),
    })
}

/// Read an id the response can echo verbatim.
///
/// JSON-RPC only defines string and number ids; any other value is dropped so
/// the error response stays well formed.
fn parse_id(value: &Value) -> Option<RequestId> {
    match value {
        Value::Number(number) => Some(RequestId::Number(number.clone())),
        Value::String(text) => Some(RequestId::String(text.clone())),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Build a successful response frame.
#[must_use]
pub fn result_frame(id: Option<&RequestId>, result: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Build an error response frame.
#[must_use]
pub fn error_frame(id: Option<&RequestId>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}

/// Build one `event` notification.
#[must_use]
pub fn event_frame(event: &impl Serialize) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn decoding_reads_a_numeric_id() {
        let request =
            decode(r#"{"jsonrpc":"2.0","id":41,"method":"config","params":{}}"#).expect("decode");
        assert_eq!(
            request.id,
            Some(RequestId::Number(serde_json::Number::from(41)))
        );
        assert_eq!(request.method, "config");
        assert_eq!(request.params, json!({}));
    }

    #[test]
    fn decoding_reads_a_string_id() {
        let request = decode(r#"{"jsonrpc":"2.0","id":"a-1","method":"tools.list","params":{}}"#)
            .expect("decode");
        assert_eq!(request.id, Some(RequestId::String("a-1".to_string())));
        assert_eq!(request.params, json!({}));
    }

    #[test]
    fn decoding_defaults_absent_params_to_null() {
        let request =
            decode(r#"{"jsonrpc":"2.0","id":1,"method":"sessions.list"}"#).expect("decode");
        assert_eq!(request.params, Value::Null);
    }

    #[test]
    fn decoding_rejects_a_frame_without_a_method() {
        let error = decode(r#"{"jsonrpc":"2.0","id":9}"#).expect_err("no method");
        assert_eq!(
            error,
            DecodeError::Shape {
                id: Some(RequestId::Number(serde_json::Number::from(9))),
                message: "request must carry a string method".to_string(),
            }
        );
    }

    #[test]
    fn decoding_rejects_a_non_string_method() {
        let error = decode(r#"{"jsonrpc":"2.0","id":9,"method":7}"#).expect_err("no method");
        assert!(matches!(error, DecodeError::Shape { .. }));
    }

    #[test]
    fn decoding_drops_an_id_it_cannot_echo() {
        let error = decode(r#"{"jsonrpc":"2.0","id":{},"method":7}"#).expect_err("no method");
        assert_eq!(
            error,
            DecodeError::Shape {
                id: None,
                message: "request must carry a string method".to_string(),
            }
        );
    }

    #[test]
    fn decoding_rejects_non_json() {
        let error = decode("not json").expect_err("syntax");
        assert!(matches!(error, DecodeError::Syntax(_)));
    }

    #[test]
    fn decoding_rejects_a_json_scalar() {
        let error = decode("7").expect_err("no method");
        assert!(matches!(error, DecodeError::Shape { id: None, .. }));
    }

    #[test]
    fn result_frames_echo_the_id() {
        let id = RequestId::Number(serde_json::Number::from(3));
        let frame = result_frame(Some(&id), &json!({"ok": true}));
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], 3);
        assert_eq!(frame["result"], json!({"ok": true}));
        assert!(frame.get("error").is_none());
    }

    #[test]
    fn result_frames_without_an_id_carry_null() {
        let frame = result_frame(None, &Value::Null);
        assert_eq!(frame["id"], Value::Null);
    }

    #[test]
    fn error_frames_carry_the_code_and_message() {
        let id = RequestId::String("q".to_string());
        let frame = error_frame(Some(&id), METHOD_NOT_FOUND, "no such method");
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], "q");
        assert_eq!(frame["error"]["code"], -32601);
        assert_eq!(frame["error"]["message"], "no such method");
        assert!(frame.get("result").is_none());
    }

    #[test]
    fn event_frames_are_notifications() {
        let frame = event_frame(&json!({"type": "text", "text": "hi"}));
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["method"], "event");
        assert_eq!(frame["params"], json!({"type": "text", "text": "hi"}));
        assert!(frame.get("id").is_none());
    }

    fn id_strategy() -> impl Strategy<Value = RequestId> {
        prop_oneof![
            any::<i64>().prop_map(|value| RequestId::Number(serde_json::Number::from(value))),
            "[A-Za-z0-9_-]{1,12}".prop_map(RequestId::String),
        ]
    }

    proptest! {
        #[test]
        fn decoding_accepts_any_valid_id_and_method(
            id in id_strategy(),
            method in "[a-z][a-z.]{0,20}",
        ) {
            let frame = json!({"jsonrpc": "2.0", "id": id, "method": method});
            let request = decode(&frame.to_string()).expect("decode");
            prop_assert_eq!(request.id, Some(id));
            prop_assert_eq!(request.method, method);
            prop_assert_eq!(request.params, Value::Null);
        }

        #[test]
        fn error_frames_echo_a_present_id(id in id_strategy(), code in any::<i32>()) {
            let frame = error_frame(Some(&id), code, "boom");
            let encoded = serde_json::to_value(&id).expect("encode id");
            prop_assert_eq!(frame.get("id"), Some(&encoded));
            prop_assert_eq!(frame["error"]["code"].as_i64(), Some(i64::from(code)));
        }
    }
}
