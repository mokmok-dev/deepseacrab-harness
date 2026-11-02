//! `Content-Length` framing for LSP over stdio.

use crate::error::LspError;
use serde_json::Value;
use std::io::BufRead;

/// Encode one message with the `Content-Length` header LSP expects.
pub fn encode_frame(message: &Value) -> Result<Vec<u8>, LspError> {
    let body = serde_json::to_vec(message)
        .map_err(|error| LspError::Protocol(format!("could not encode frame: {error}")))?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one framed message, skipping unknown headers.
pub fn read_frame(reader: &mut impl BufRead) -> Result<Value, LspError> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| LspError::Protocol(format!("read from server: {error}")))?;
        if read == 0 {
            return Err(LspError::Protocol(
                "server closed stdout before answering".to_string(),
            ));
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            let Some(length) = length else {
                return Err(LspError::Protocol(
                    "frame is missing Content-Length".to_string(),
                ));
            };
            let mut body = vec![0_u8; length];
            reader
                .read_exact(&mut body)
                .map_err(|error| LspError::Protocol(format!("read from server: {error}")))?;
            return serde_json::from_slice(&body).map_err(|error| {
                LspError::Protocol(format!("frame body is not valid JSON: {error}"))
            });
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().map_err(|_| {
                    LspError::Protocol(format!("invalid Content-Length header: {value}"))
                })?);
            }
        }
    }
}

/// The value one response frame carries, when it answers `id`.
///
/// Notifications, server-to-client requests, and responses to other ids yield
/// `None` so the caller keeps reading until its own answer arrives.
#[must_use]
pub fn response_result(frame: &Value, id: u64) -> Option<Result<Value, LspError>> {
    if frame.get("method").is_some() {
        return None;
    }
    if frame.get("id").and_then(Value::as_u64) != Some(id) {
        return None;
    }
    if let Some(error) = frame.get("error") {
        return Some(Err(LspError::Rpc(rpc_message(error))));
    }
    Some(Ok(frame.get("result").cloned().unwrap_or(Value::Null)))
}

fn rpc_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn frames(messages: &[Value]) -> Vec<u8> {
        messages
            .iter()
            .flat_map(|message| encode_frame(message).expect("encodes"))
            .collect()
    }

    #[test]
    fn a_frame_carries_a_byte_length_and_the_body() {
        let frame = encode_frame(&json!({"a": 1})).expect("encodes");
        assert_eq!(frame, b"Content-Length: 7\r\n\r\n{\"a\":1}");
    }

    #[test]
    fn content_length_counts_bytes_not_characters() {
        let body = r#"{"text":"é"}"#;
        let frame = encode_frame(&json!({"text": "é"})).expect("encodes");
        assert_eq!(
            frame,
            format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
        );
    }

    #[test]
    fn frames_round_trip_in_order() {
        let messages = vec![json!({"id": 1, "result": {}}), json!({"id": 2})];
        let mut reader = Cursor::new(frames(&messages));
        assert_eq!(read_frame(&mut reader).expect("first"), messages[0]);
        assert_eq!(read_frame(&mut reader).expect("second"), messages[1]);
    }

    #[test]
    fn header_names_are_matched_without_case() {
        let mut reader = Cursor::new(b"content-length: 2\r\n\r\n{}".to_vec());
        assert_eq!(read_frame(&mut reader).expect("reads"), json!({}));
    }

    #[test]
    fn unknown_and_headerless_lines_are_skipped() {
        let mut reader = Cursor::new(
            b"X-Custom: yes\r\nnot a header\r\nContent-Length: 2\r\n\r\n{} trailing".to_vec(),
        );
        assert_eq!(read_frame(&mut reader).expect("reads"), json!({}));
    }

    #[test]
    fn a_missing_length_is_rejected() {
        let mut reader = Cursor::new(b"X-Custom: yes\r\n\r\n{}".to_vec());
        assert_eq!(
            read_frame(&mut reader),
            Err(LspError::Protocol(
                "frame is missing Content-Length".to_string()
            ))
        );
    }

    #[test]
    fn a_malformed_length_is_rejected() {
        for value in ["abc", "-1", "1.5"] {
            let mut reader =
                Cursor::new(format!("Content-Length: {value}\r\n\r\n{{}}").into_bytes());
            match read_frame(&mut reader) {
                Err(LspError::Protocol(message)) => {
                    assert!(message.contains("invalid Content-Length"));
                }
                other => panic!("expected a header failure for {value}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_truncated_body_is_rejected() {
        let mut reader = Cursor::new(b"Content-Length: 10\r\n\r\n{}".to_vec());
        assert!(matches!(
            read_frame(&mut reader),
            Err(LspError::Protocol(message)) if message.contains("read from server")
        ));
    }

    #[test]
    fn an_invalid_body_is_rejected() {
        let mut reader = Cursor::new(b"Content-Length: 1\r\n\r\n{".to_vec());
        assert!(matches!(
            read_frame(&mut reader),
            Err(LspError::Protocol(message)) if message.contains("not valid JSON")
        ));
    }

    #[test]
    fn eof_before_a_frame_is_rejected() {
        let mut reader = Cursor::new(Vec::new());
        assert_eq!(
            read_frame(&mut reader),
            Err(LspError::Protocol(
                "server closed stdout before answering".to_string()
            ))
        );
    }

    #[test]
    fn a_matching_response_yields_its_result() {
        assert_eq!(
            response_result(&json!({"id": 3, "result": {"ok": true}}), 3),
            Some(Ok(json!({"ok": true})))
        );
        assert_eq!(response_result(&json!({"id": 3}), 3), Some(Ok(Value::Null)));
    }

    #[test]
    fn a_matching_error_yields_the_message() {
        assert_eq!(
            response_result(
                &json!({"id": 3, "error": {"code": -1, "message": "bad"}}),
                3
            ),
            Some(Err(LspError::Rpc("bad".to_string())))
        );
        assert_eq!(
            response_result(&json!({"id": 3, "error": "plain"}), 3),
            Some(Err(LspError::Rpc("\"plain\"".to_string())))
        );
    }

    #[test]
    fn notifications_and_other_ids_are_skipped() {
        assert_eq!(
            response_result(&json!({"method": "textDocument/publishDiagnostics"}), 3),
            None
        );
        assert_eq!(response_result(&json!({"id": 4, "result": {}}), 3), None);
        assert_eq!(
            response_result(
                &json!({"id": 3, "method": "window/showMessage", "params": {}}),
                3
            ),
            None
        );
        assert_eq!(response_result(&json!({"result": {}}), 3), None);
        assert_eq!(response_result(&json!({"id": "3", "result": {}}), 3), None);
    }
}
