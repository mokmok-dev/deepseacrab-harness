#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions
)]

//! Messages API adapter for any Messages-compatible endpoint.
//!
//! The network surface is deliberately small: one POST, one byte stream, and
//! a translator that turns decoded SSE frames into [`dsh_core::StreamEvent`]s.

pub mod retry;
pub mod sse;
pub mod wire;

use bytes::Bytes;
use dsh_core::{LlmError, LlmErrorCode, LlmProvider, LlmRequest, LlmStream, StreamEvent};
use futures_util::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub use retry::RetryPolicy;
pub use sse::{SseEvent, SseParser};
pub use wire::{
    OutputConfig, Translator, WireBlock, WireEffort, WireInput, WireMessage, WireRequest, WireRole,
    WireThinking, WireTool, serialize_request,
};

/// Endpoint and credential settings for one Messages route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagesConfig {
    /// Route name stamped into diagnostics and durable messages.
    pub name: String,
    /// Messages API base URL, with or without a trailing `/v1`.
    pub base_url: String,
    /// Credential sent as `x-api-key`.
    pub api_key: String,
    /// Whole-request timeout.
    pub timeout: Duration,
}

/// Messages provider backed by one `reqwest` client.
#[derive(Debug, Clone)]
pub struct MessagesProvider {
    client: reqwest::Client,
    config: MessagesConfig,
}

impl MessagesProvider {
    /// Validate configuration and build the shared HTTP client.
    pub fn new(config: MessagesConfig) -> Result<Self, LlmError> {
        if config.base_url.trim().trim_end_matches('/').is_empty() {
            return Err(LlmError::invalid_request("base_url must not be empty"));
        }
        if reqwest::Url::parse(&config.base_url).is_err() {
            return Err(LlmError::invalid_request(
                "base_url must be an absolute URL",
            ));
        }
        if config.api_key.is_empty() {
            return Err(LlmError::missing_credential("api_key must not be empty"));
        }
        if config.timeout.is_zero() {
            return Err(LlmError::invalid_request(
                "timeout must be greater than zero",
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| LlmError::transport(error.to_string()))?;
        Ok(Self { client, config })
    }

    /// The Messages endpoint derived from the configured base.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for MessagesProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmStream, LlmError> {
        let body = serialize_request(&request)?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let builder = self
            .client
            .post(self.endpoint())
            .header("x-api-key", self.config.api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body);
        let response = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            result = builder.send() => result.map_err(|error| LlmError::transport(error.to_string()))?,
        };
        let status = response.status();
        if !status.is_success() {
            let detail = tokio::select! {
                () = cancel.cancelled() => return Err(cancelled()),
                result = response.text() => match result {
                    Ok(detail) => detail,
                    Err(error) => error.to_string(),
                },
            };
            return Err(LlmError::from_status(status.as_u16(), detail));
        }
        Ok(Box::pin(stream_events(
            Box::pin(response.bytes_stream()),
            cancel,
        )))
    }
}

fn cancelled() -> LlmError {
    LlmError::new(LlmErrorCode::Transport, "Messages request cancelled", false)
}

struct StreamState {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    parser: SseParser,
    translator: Translator,
    cancel: CancellationToken,
    queue: VecDeque<StreamEvent>,
    error: Option<LlmError>,
    done: bool,
}

fn stream_events(
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<StreamEvent, LlmError>> + Send {
    futures_util::stream::unfold(
        StreamState {
            inner,
            parser: SseParser::new(),
            translator: Translator::new(),
            cancel,
            queue: VecDeque::new(),
            error: None,
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.queue.pop_front() {
                    return Some((Ok(event), state));
                }
                if let Some(error) = state.error.take() {
                    state.done = true;
                    return Some((Err(error), state));
                }
                if state.done {
                    return None;
                }
                if state.cancel.is_cancelled() {
                    state.error = Some(cancelled());
                    continue;
                }
                let cancellation = state.cancel.clone();
                let next = tokio::select! {
                    () = cancellation.cancelled() => {
                        state.error = Some(cancelled());
                        continue;
                    }
                    next = state.inner.next() => next,
                };
                match next {
                    None => {
                        state.done = true;
                        if let Err(error) = state.translator.ensure_finished() {
                            state.error = Some(error);
                        }
                    }
                    Some(Err(error)) => {
                        state.done = true;
                        state.error = Some(LlmError::transport(error.to_string()));
                    }
                    Some(Ok(chunk)) => {
                        let events = match state.parser.push(&chunk) {
                            Ok(events) => events,
                            Err(error) => {
                                state.done = true;
                                state.error = Some(error);
                                continue;
                            }
                        };
                        for event in &events {
                            match state.translator.push(event) {
                                Ok(translated) => state.queue.extend(translated),
                                Err(error) => {
                                    state.done = true;
                                    state.error = Some(error);
                                    break;
                                }
                            }
                        }
                        if state.translator.is_finished() {
                            state.done = true;
                        }
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::{FinishReason, ModelId, TokenUsage, ToolSchema};
    use futures_util::StreamExt;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn config() -> MessagesConfig {
        MessagesConfig {
            name: "deepseek".to_string(),
            base_url: "https://api.deepseek.com/anthropic".to_string(),
            api_key: "secret".to_string(),
            timeout: Duration::from_secs(30),
        }
    }

    fn provider(base_url: &str) -> MessagesProvider {
        MessagesProvider::new(MessagesConfig {
            base_url: base_url.to_string(),
            ..config()
        })
        .expect("provider")
    }

    #[test]
    fn endpoint_appends_v1_once() {
        assert_eq!(
            provider("https://api.deepseek.com/anthropic").endpoint(),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            provider("https://api.deepseek.com/anthropic/v1").endpoint(),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            provider("https://api.deepseek.com/anthropic/v1/").endpoint(),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            provider("https://api.deepseek.com/anthropic///").endpoint(),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn provider_name_comes_from_its_config() {
        let provider = MessagesProvider::new(MessagesConfig {
            name: "gw".to_string(),
            ..config()
        })
        .expect("provider");
        assert_eq!(provider.name(), "gw");
    }

    #[test]
    fn configuration_is_validated() {
        let error = MessagesProvider::new(MessagesConfig {
            base_url: "   ".to_string(),
            ..config()
        })
        .expect_err("empty base");
        assert_eq!(error.code, LlmErrorCode::InvalidRequest);

        let error = MessagesProvider::new(MessagesConfig {
            base_url: "///".to_string(),
            ..config()
        })
        .expect_err("slash base");
        assert_eq!(error.code, LlmErrorCode::InvalidRequest);

        let error = MessagesProvider::new(MessagesConfig {
            base_url: "not a url".to_string(),
            ..config()
        })
        .expect_err("invalid url");
        assert_eq!(error.code, LlmErrorCode::InvalidRequest);

        let error = MessagesProvider::new(MessagesConfig {
            api_key: String::new(),
            ..config()
        })
        .expect_err("missing key");
        assert_eq!(error.code, LlmErrorCode::MissingCredential);

        let error = MessagesProvider::new(MessagesConfig {
            timeout: Duration::ZERO,
            ..config()
        })
        .expect_err("zero timeout");
        assert_eq!(error.code, LlmErrorCode::InvalidRequest);
    }

    fn request() -> LlmRequest {
        LlmRequest {
            model: ModelId::parse("deepseek-flash").expect("model"),
            system: Some("be useful".to_string()),
            messages: vec![dsh_core::Message::user("hello")],
            tools: Vec::new(),
            max_tokens: 128,
            effort: dsh_core::ReasoningEffort::High,
            temperature: None,
            stop: Vec::new(),
        }
    }

    #[tokio::test]
    async fn already_cancelled_requests_are_not_dispatched() {
        let provider = provider("https://127.0.0.1:1");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = provider
            .stream(request(), cancel)
            .await
            .err()
            .expect("cancelled");
        assert_eq!(error.code, LlmErrorCode::Transport);
    }

    #[tokio::test]
    async fn invalid_requests_fail_before_dispatch() {
        let provider = provider("https://127.0.0.1:1");
        let mut request = request();
        request.max_tokens = 0;
        let error = provider
            .stream(request, CancellationToken::new())
            .await
            .err()
            .expect("invalid");
        assert_eq!(error.code, LlmErrorCode::InvalidRequest);
    }

    struct Captured {
        request_line: String,
        headers: String,
        body: String,
    }

    fn header_end(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Captured {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("read");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(end) = header_end(&buffer) {
                let headers = String::from_utf8_lossy(&buffer[..end]).to_string();
                if buffer.len() >= end + 4 + content_length(&headers) {
                    break;
                }
            }
        }
        let end = header_end(&buffer).expect("headers");
        let headers = String::from_utf8_lossy(&buffer[..end]).to_string();
        let body = String::from_utf8_lossy(&buffer[end + 4..]).to_string();
        let request_line = headers.lines().next().unwrap_or_default().to_string();
        Captured {
            request_line,
            headers,
            body,
        }
    }

    async fn serve(response: String) -> (String, tokio::task::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let captured = read_request(&mut socket).await;
            socket.write_all(response.as_bytes()).await.expect("write");
            socket.shutdown().await.expect("shutdown");
            captured
        });
        (format!("http://{address}"), handle)
    }

    async fn serve_then_hold(response: String) -> (String, tokio::task::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let captured = read_request(&mut socket).await;
            socket.write_all(response.as_bytes()).await.expect("write");
            tokio::time::sleep(Duration::from_secs(5)).await;
            captured
        });
        (format!("http://{address}"), handle)
    }

    fn sse_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
    }

    const STREAM: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    async fn collect(mut stream: LlmStream) -> Vec<Result<StreamEvent, LlmError>> {
        let mut items = Vec::new();
        while let Some(item) = stream.next().await {
            items.push(item);
        }
        items
    }

    #[tokio::test]
    async fn posts_the_serialized_request_and_decodes_the_stream() {
        let (base, server) = serve(sse_response(STREAM)).await;
        let provider = provider(&base);
        let mut request = request();
        request.effort = dsh_core::ReasoningEffort::Max;
        request.tools = vec![
            ToolSchema::new("read", "Read a file", serde_json::json!({"type": "object"}))
                .expect("schema"),
        ];
        request.temperature = Some(0.2);
        request.stop = vec!["STOP".to_string()];
        let stream = provider
            .stream(request, CancellationToken::new())
            .await
            .expect("stream");
        let items = collect(stream).await;
        assert_eq!(
            items,
            vec![
                Ok(StreamEvent::TextDelta {
                    text: "Hi".to_string(),
                }),
                Ok(StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 2,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                }),
                Ok(StreamEvent::Finish {
                    reason: FinishReason::Stop,
                }),
            ]
        );

        let captured = server.await.expect("server");
        assert_eq!(captured.request_line, "POST /v1/messages HTTP/1.1");
        let headers = captured.headers.to_lowercase();
        assert!(headers.contains("x-api-key: secret"));
        assert!(headers.contains("anthropic-version: 2023-06-01"));
        assert!(headers.contains("content-type: application/json"));
        assert!(headers.contains("accept: text/event-stream"));

        let payload: Value = serde_json::from_str(&captured.body).expect("json");
        assert_eq!(payload["model"], "deepseek-flash");
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["max_tokens"], 128);
        assert_eq!(payload["system"], "be useful");
        assert_eq!(payload["thinking"], serde_json::json!({"type": "enabled"}));
        assert_eq!(
            payload["output_config"],
            serde_json::json!({"effort": "max"})
        );
        assert_eq!(payload["temperature"], 0.2);
        assert_eq!(payload["stop_sequences"], serde_json::json!(["STOP"]));
        assert_eq!(payload["tools"][0]["name"], "read");
        assert_eq!(
            payload["messages"][0]["content"][0],
            serde_json::json!({"type": "text", "text": "hello"})
        );
        assert!(payload["messages"][0]["content"][0].get("text").is_some());
    }

    #[tokio::test]
    async fn http_failures_are_classified_from_status_and_body() {
        for (status_line, status, code) in [
            ("401 Unauthorized", 401, LlmErrorCode::Unauthorized),
            ("400 Bad Request", 400, LlmErrorCode::InvalidRequest),
            ("429 Too Many Requests", 429, LlmErrorCode::RateLimited),
            ("503 Service Unavailable", 503, LlmErrorCode::Server),
        ] {
            let body = r#"{"error":{"message":"detail"}}"#;
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let (base, server) = serve(response).await;
            let provider = provider(&base);
            let error = provider
                .stream(request(), CancellationToken::new())
                .await
                .err()
                .expect("error");
            assert_eq!(error.code, code);
            assert_eq!(error.status, Some(status));
            assert!(error.message.contains("detail"));
            server.await.expect("server");
        }
    }

    #[tokio::test]
    async fn a_stream_that_ends_early_reports_stream_closed() {
        let body = "data: {\"type\":\"message_start\",\"message\":{}}\n\n";
        let (base, server) = serve(sse_response(body)).await;
        let stream = provider(&base)
            .stream(request(), CancellationToken::new())
            .await
            .expect("stream");
        let items = collect(stream).await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].as_ref().expect_err("closed").code,
            LlmErrorCode::StreamClosed
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn cancelling_the_token_stops_the_stream() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        );
        let (base, server) = serve_then_hold(sse_response(body)).await;
        let provider = provider(&base);
        let cancel = CancellationToken::new();
        let mut stream = provider
            .stream(request(), cancel.clone())
            .await
            .expect("stream");
        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("first")
            .expect("item")
            .expect("event");
        assert_eq!(
            first,
            StreamEvent::TextDelta {
                text: "Hi".to_string()
            }
        );
        cancel.cancel();
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("cancelled");
        assert_eq!(
            next.expect("item").expect_err("cancelled").code,
            LlmErrorCode::Transport
        );
        server.abort();
    }
}
