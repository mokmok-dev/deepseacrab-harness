#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Browser client and JSON/SSE API for the headless harness.
//!
//! [`router`] is a pure function of [`WebState`], so the whole surface can be
//! driven with `tower::ServiceExt::oneshot` without binding a socket.

mod agui;
mod chat;
mod projects;

pub use chat::RunRegistry;
pub use projects::{Project, ProjectError, ProjectStore, ProjectView};

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dsh_agent::{AgentConfig, AgentError};
use dsh_core::{
    Clock, EventRecord, IdError, LlmProvider, SessionId, SessionStore, SessionSummary, StoreError,
    ToolRegistry, ToolSchema,
};
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// The chat page served at `/`.
const INDEX_HTML: &str = include_str!("../static/index.html");

/// The client posts its whole transcript, which grows with the session.
const CHAT_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Everything one HTTP request may use.
#[derive(Clone)]
pub struct WebState {
    /// Durable session storage.
    pub store: Arc<dyn SessionStore>,
    /// Model route behind every run.
    pub provider: Arc<dyn LlmProvider>,
    /// Tools offered to the model.
    pub tools: ToolRegistry,
    /// Per-run model, policy, and workspace settings.
    pub config: AgentConfig,
    /// Wall clock stamped on durable writes.
    pub clock: Arc<dyn Clock>,
    pub runs: RunRegistry,
}

/// Failures that keep the server from running.
#[derive(Debug, Error)]
pub enum WebError {
    /// The listen address could not be bound.
    #[error("cannot bind {addr}: {source}")]
    Bind {
        /// Requested address.
        addr: SocketAddr,
        /// Operating-system failure.
        #[source]
        source: std::io::Error,
    },
    /// The listener failed while serving.
    #[error("server failure: {0}")]
    Serve(#[source] std::io::Error),
}

/// A request the API refused or could not answer.
#[derive(Debug, Error)]
pub(crate) enum ApiError {
    /// The request body or path did not match the contract.
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    /// Durable storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<IdError> for ApiError {
    fn from(error: IdError) -> Self {
        Self::BadRequest(format!("invalid session id: {error}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Store(StoreError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}

/// Build the HTTP surface for one harness deployment.
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", get(session_log))
        .route("/api/tools", get(list_tools))
        .route(
            "/api/chat",
            post(chat::chat).layer(DefaultBodyLimit::max(CHAT_BODY_LIMIT)),
        )
        .with_state(state)
}

/// Bind `bind` and serve until `cancel` is cancelled.
pub async fn serve(
    state: WebState,
    bind: SocketAddr,
    cancel: CancellationToken,
) -> Result<(), WebError> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|source| WebError::Bind { addr: bind, source })?;
    let runs = state.runs.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
            runs.cancel_all();
        })
        .await
        .map_err(WebError::Serve)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> &'static str {
    "ok"
}

async fn list_sessions(
    State(state): State<WebState>,
) -> Result<Json<Vec<SessionSummary>>, ApiError> {
    Ok(Json(state.store.list()?))
}

async fn session_log(
    State(state): State<WebState>,
    Path(raw): Path<String>,
) -> Result<Json<Vec<EventRecord>>, ApiError> {
    let session_id = SessionId::parse(raw)?;
    Ok(Json(state.store.load(&session_id)?))
}

async fn list_tools(State(state): State<WebState>) -> Json<Vec<ToolSchema>> {
    Json(state.tools.schemas())
}

/// The stable machine code for a failure that skipped the turn driver.
pub(crate) const fn failure_code(error: &AgentError) -> &'static str {
    match error {
        AgentError::Store(_) => "STORE_ERROR",
        AgentError::InvalidProviderName(_) => "INVALID_PROVIDER",
        _ => "AGENT_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::CONTENT_TYPE;
    use dsh_core::{
        EventSeq, FinishReason, FixedClock, LlmError, LlmRequest, LlmStream, ModelId, SessionEvent,
        SessionHeader, StreamEvent, Tool, ToolCallId, ToolContext, ToolOutcome, TurnOutcome,
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;
    use tower::ServiceExt;

    #[derive(Default)]
    struct MockState {
        sessions: BTreeMap<SessionId, (SessionHeader, Vec<EventRecord>)>,
    }

    #[derive(Default)]
    struct MockStore {
        state: Mutex<MockState>,
    }

    #[allow(clippy::significant_drop_tightening)]
    impl SessionStore for MockStore {
        fn create(&self, header: &SessionHeader) -> Result<(), StoreError> {
            let mut state = self.state.lock().expect("lock");
            if state.sessions.contains_key(&header.id) {
                return Err(StoreError::AlreadyExists(header.id.clone()));
            }
            let record = EventRecord {
                seq: EventSeq::FIRST,
                at: header.created_at,
                event: SessionEvent::SessionCreated {
                    cwd: header.cwd.clone(),
                    created_at: header.created_at,
                    format_version: header.format_version,
                },
            };
            state
                .sessions
                .insert(header.id.clone(), (header.clone(), vec![record]));
            Ok(())
        }

        fn append(
            &self,
            session_id: &SessionId,
            at: i64,
            event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            let mut state = self.state.lock().expect("lock");
            let Some((_, events)) = state.sessions.get_mut(session_id) else {
                return Err(StoreError::NotFound(session_id.clone()));
            };
            let record = EventRecord {
                seq: EventSeq::new(u64::try_from(events.len()).expect("length")),
                at,
                event,
            };
            events.push(record.clone());
            Ok(record)
        }

        fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
            let state = self.state.lock().expect("lock");
            state
                .sessions
                .get(session_id)
                .map(|(_, events)| events.clone())
                .ok_or_else(|| StoreError::NotFound(session_id.clone()))
        }

        fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
            let state = self.state.lock().expect("lock");
            Ok(state
                .sessions
                .values()
                .map(|(header, events)| SessionSummary {
                    id: header.id.clone(),
                    created_at: header.created_at,
                    cwd: header.cwd.clone(),
                    title: header.title.clone(),
                    event_count: u64::try_from(events.len()).expect("length"),
                })
                .collect())
        }

        fn has(&self, session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(self
                .state
                .lock()
                .expect("lock")
                .sessions
                .contains_key(session_id))
        }

        fn delete(&self, session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(self
                .state
                .lock()
                .expect("lock")
                .sessions
                .remove(session_id)
                .is_some())
        }
    }

    struct BrokenStore;

    impl SessionStore for BrokenStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
            Ok(())
        }

        fn append(
            &self,
            _session_id: &SessionId,
            _at: i64,
            _event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            Err(StoreError::Backend("disk full".to_string()))
        }

        fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
            Ok(Vec::new())
        }

        fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
            Err(StoreError::Backend("disk full".to_string()))
        }

        fn has(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }

        fn delete(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    struct ScriptedProvider {
        name: &'static str,
        responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Self {
            Self {
                name: "scripted",
                responses: Mutex::new(responses.into()),
            }
        }

        fn named(name: &'static str) -> Self {
            Self {
                name,
                responses: Mutex::new(VecDeque::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn stream(
            &self,
            _request: LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or_default();
            Ok(Box::pin(tokio_stream::iter(response)))
        }
    }

    struct PendingProvider;

    #[async_trait]
    impl LlmProvider for PendingProvider {
        fn name(&self) -> &'static str {
            "pending"
        }

        async fn stream(
            &self,
            _request: LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            Ok(Box::pin(tokio_stream::pending()))
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("echo", "echo the arguments", json!({"type": "object"}))
                .expect("schema")
        }

        fn concurrency_safe(&self) -> bool {
            true
        }

        async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
            ToolOutcome::text(arguments.to_string())
        }
    }

    fn config() -> AgentConfig {
        let mut config = AgentConfig::new(
            ModelId::parse("test-model").expect("model"),
            PathBuf::from("/work"),
        );
        config.system_prompt = "be useful".to_string();
        config.max_tokens = 128;
        config.max_steps = 4;
        config.max_retries = 0;
        config
    }

    fn tools() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.insert(Arc::new(EchoTool)).expect("tool");
        tools
    }

    fn web_state(store: Arc<dyn SessionStore>, provider: Arc<dyn LlmProvider>) -> WebState {
        WebState {
            store,
            provider,
            tools: tools(),
            config: config(),
            clock: Arc::new(FixedClock(1_000)),
            runs: chat::RunRegistry::new(),
        }
    }

    fn text_provider(text: &str) -> ScriptedProvider {
        ScriptedProvider::new(vec![vec![
            Ok(StreamEvent::TextDelta {
                text: text.to_string(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::Stop,
            }),
        ]])
    }

    fn tool_then_text_provider() -> ScriptedProvider {
        let call = ToolCallId::parse("call-1").expect("call id");
        ScriptedProvider::new(vec![
            vec![
                Ok(StreamEvent::ToolCallStart {
                    id: call.clone(),
                    name: "echo".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: call,
                    arguments: r#"{"text":"hi"}"#.to_string(),
                }),
                Ok(StreamEvent::Finish {
                    reason: FinishReason::ToolCalls,
                }),
            ],
            vec![
                Ok(StreamEvent::TextDelta {
                    text: "done".to_string(),
                }),
                Ok(StreamEvent::Finish {
                    reason: FinishReason::Stop,
                }),
            ],
        ])
    }

    async fn execute(state: WebState, request: Request<Body>) -> Response {
        router(state).oneshot(request).await.expect("router")
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request")
    }

    fn post_request(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    async fn body_text(response: Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    fn json_body(text: &str) -> Value {
        serde_json::from_str(text).expect("json")
    }

    fn frames(text: &str) -> Vec<Value> {
        text.split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(|frame| {
                let data = frame.strip_prefix("data: ").expect("data frame");
                serde_json::from_str(data).expect("json frame")
            })
            .collect()
    }

    fn event_types(frames: &[Value]) -> Vec<&str> {
        frames
            .iter()
            .map(|frame| frame["type"].as_str().expect("type"))
            .collect()
    }

    #[tokio::test]
    async fn index_serves_the_chat_page() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let response = execute(state, get_request("/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        let html = body_text(response).await;
        assert!(
            html.contains("id=\"root\""),
            "page must mount the React client"
        );
        assert!(
            html.contains("/api/chat"),
            "page must stream its runs from the AG-UI route"
        );
    }

    #[tokio::test]
    async fn health_reports_ok() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let response = execute(state, get_request("/api/health")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "ok");
    }

    #[tokio::test]
    async fn sessions_listing_returns_summaries() {
        let store = Arc::new(MockStore::default());
        store
            .create(&SessionHeader::new(
                SessionId::parse("session-known").expect("id"),
                7,
                Some("/work".to_string()),
            ))
            .expect("create");
        let state = web_state(store, Arc::new(text_provider("hi")));
        let response = execute(state, get_request("/api/sessions")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(&body_text(response).await),
            json!([{
                "id": "session-known",
                "created_at": 7,
                "cwd": "/work",
                "title": null,
                "event_count": 1,
            }])
        );
    }

    #[tokio::test]
    async fn session_log_returns_durable_events() {
        let store = Arc::new(MockStore::default());
        store
            .create(&SessionHeader::new(
                SessionId::parse("session-known").expect("id"),
                7,
                None,
            ))
            .expect("create");
        let state = web_state(store, Arc::new(text_provider("hi")));
        let response = execute(state, get_request("/api/sessions/session-known")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = json_body(&body_text(response).await);
        assert_eq!(payload.as_array().expect("array").len(), 1);
        assert_eq!(payload[0]["seq"], 0);
        assert_eq!(payload[0]["at"], 7);
        assert_eq!(payload[0]["event"]["type"], "session_created");
        assert_eq!(payload[0]["event"]["created_at"], 7);
    }

    #[tokio::test]
    async fn unknown_sessions_report_not_found() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let response = execute(state, get_request("/api/sessions/session-missing")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            json_body(&body_text(response).await)["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
    }

    #[tokio::test]
    async fn malformed_session_ids_are_rejected() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let uri = format!("/api/sessions/{}", "x".repeat(300));
        let response = execute(state, get_request(&uri)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_body(&body_text(response).await)["error"]
                .as_str()
                .expect("error")
                .contains("invalid session id")
        );
    }

    #[tokio::test]
    async fn store_failures_report_server_errors() {
        let state = web_state(Arc::new(BrokenStore), Arc::new(text_provider("hi")));
        let response = execute(state, get_request("/api/sessions")).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            json_body(&body_text(response).await)["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
    }

    #[tokio::test]
    async fn tool_listing_returns_schemas() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let response = execute(state, get_request("/api/tools")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = json_body(&body_text(response).await);
        assert_eq!(payload[0]["name"], "echo");
        assert_eq!(payload[0]["parameters"]["type"], "object");
        assert!(
            payload[0]["description"]
                .as_str()
                .is_some_and(|description| !description.is_empty())
        );
    }

    fn agui_body(thread: &str, run: &str, text: &str) -> String {
        json!({
            "threadId": thread,
            "runId": run,
            "messages": [{ "role": "user", "content": text }],
        })
        .to_string()
    }

    fn agui_request(body: &str) -> Request<Body> {
        post_request("/api/chat", body)
    }

    async fn wait_for_event(store: &MockStore, session: &SessionId, kind: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let reached = store
                    .load(session)
                    .is_ok_and(|records| records.iter().any(|record| record.event.kind() == kind));
                if reached {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("event {kind} must arrive"));
    }

    async fn wait_for_turn_end(store: &MockStore, session: &SessionId) -> TurnOutcome {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outcome = store.load(session).ok().and_then(|records| {
                    records.iter().find_map(|record| match &record.event {
                        SessionEvent::TurnEnded { outcome, .. } => Some(outcome.clone()),
                        _ => None,
                    })
                });
                if let Some(outcome) = outcome {
                    return outcome;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("turn must end")
    }

    #[tokio::test]
    async fn agui_streams_a_text_turn() {
        let store = Arc::new(MockStore::default());
        let state = web_state(store.clone(), Arc::new(text_provider("hello")));
        let response = execute(
            state,
            agui_request(&agui_body("session-chat", "run-1", "hi")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let frames = frames(&body_text(response).await);
        assert_eq!(
            event_types(&frames),
            [
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "RUN_FINISHED",
            ]
        );
        assert_eq!(frames[0]["threadId"], "session-chat");
        assert_eq!(frames[0]["runId"], "run-1");
        assert_eq!(frames[1]["role"], "assistant");
        assert_eq!(frames[2]["messageId"], frames[1]["messageId"]);
        assert_eq!(frames[2]["delta"], "hello");
        assert_eq!(frames[3]["messageId"], frames[1]["messageId"]);
        assert_eq!(frames[4]["threadId"], "session-chat");
        assert_eq!(frames[4]["runId"], "run-1");

        let session_id = SessionId::parse("session-chat").expect("id");
        let log = store.load(&session_id).expect("log");
        assert!(
            log.iter()
                .any(|record| record.event.kind() == "user_message")
        );
        assert!(log.iter().any(|record| record.event.kind() == "turn_ended"));
    }

    #[tokio::test]
    async fn agui_streams_tool_calls_before_the_text_turn() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(tool_then_text_provider()),
        );
        let body = json!({
            "threadId": "session-chat",
            "runId": "run-1",
            "messages": [{ "role": "user", "content": "use the echo tool" }],
            "state": {},
            "tools": [],
            "context": [],
            "forwardedProps": {},
        })
        .to_string();
        let response = execute(state, agui_request(&body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&body_text(response).await);
        assert_eq!(
            event_types(&frames),
            [
                "RUN_STARTED",
                "TOOL_CALL_START",
                "TOOL_CALL_ARGS",
                "TOOL_CALL_END",
                "TOOL_CALL_RESULT",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "RUN_FINISHED",
            ]
        );
        assert_eq!(frames[1]["toolCallId"], "call-1");
        assert_eq!(frames[1]["toolCallName"], "echo");
        assert_eq!(frames[2]["toolCallId"], "call-1");
        assert_eq!(frames[2]["delta"], r#"{"text":"hi"}"#);
        assert_eq!(frames[3]["toolCallId"], "call-1");
        assert_eq!(frames[4]["toolCallId"], "call-1");
        assert_eq!(frames[4]["role"], "tool");
        assert_eq!(frames[4]["content"], r#"{"text":"hi"}"#);
        assert!(frames[4].get("metadata").is_none());
        assert_eq!(frames[6]["delta"], "done");
    }

    #[tokio::test]
    async fn agui_rejects_invalid_requests() {
        for body in [
            r#"{"threadId":"session-chat","runId":"run-1","messages":[]}"#,
            r#"{"threadId":"session-chat","runId":"run-1","messages":[{"role":"user","content":"  \n "}]}"#,
            r#"{"threadId":"session-chat","runId":"run-1","messages":[{"role":"user","parts":[{"type":"text","text":" "}]}]}"#,
            r#"{"threadId":"session-chat","runId":"run-1"}"#,
            r#"{"threadId":"session-chat","runId":"","messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"threadId":"","runId":"run-1","messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"threadId":"session-chat","runId":"run-1","messages":[{"role":"user","content":"hi"}"#,
            "not json",
        ] {
            let state = web_state(
                Arc::new(MockStore::default()),
                Arc::new(text_provider("hi")),
            );
            let response = execute(state, agui_request(body)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "body {body}");
            assert!(
                json_body(&body_text(response).await)["error"]
                    .as_str()
                    .is_some_and(|error| !error.is_empty()),
                "body {body}"
            );
        }
    }

    #[tokio::test]
    async fn agui_reports_setup_failures_as_run_errors() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(ScriptedProvider::named("")),
        );
        let response = execute(
            state,
            agui_request(&agui_body("session-chat", "run-1", "hi")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&body_text(response).await);
        assert_eq!(event_types(&frames), ["RUN_STARTED", "RUN_ERROR"]);
        assert_eq!(frames[1]["code"], "INVALID_PROVIDER");
        assert!(
            frames[1]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
    }

    #[tokio::test]
    async fn agui_reports_store_failures_as_run_errors() {
        let state = web_state(Arc::new(BrokenStore), Arc::new(text_provider("hi")));
        let response = execute(
            state,
            agui_request(&agui_body("session-chat", "run-1", "hi")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&body_text(response).await);
        assert_eq!(event_types(&frames), ["RUN_STARTED", "RUN_ERROR"]);
        assert_eq!(frames[1]["code"], "STORE_ERROR");
        assert!(
            frames[1]["message"]
                .as_str()
                .is_some_and(|message| message.contains("disk full"))
        );
    }

    #[tokio::test]
    async fn agui_rejects_a_second_run_on_a_busy_session() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let session_id = SessionId::parse("session-chat").expect("id");
        let held = state.runs.reserve(&session_id).expect("reserved");
        let response = execute(
            state,
            agui_request(&agui_body("session-chat", "run-1", "hi")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            json_body(&body_text(response).await)["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
        drop(held);
    }

    #[tokio::test]
    async fn agui_dropping_the_body_cancels_the_run() {
        let store = Arc::new(MockStore::default());
        let state = web_state(store.clone(), Arc::new(PendingProvider));
        let response = execute(
            state,
            agui_request(&agui_body("session-chat", "run-1", "hi")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let session_id = SessionId::parse("session-chat").expect("id");
        wait_for_event(&store, &session_id, "turn_started").await;

        drop(response.into_body());

        let outcome = wait_for_turn_end(&store, &session_id).await;
        assert!(
            matches!(outcome, TurnOutcome::Cancelled { .. }),
            "dropping the body must cancel the run, got {outcome:?}"
        );
    }

    #[test]
    fn run_registry_reserves_one_run_per_session() {
        let registry = RunRegistry::new();
        let session = SessionId::parse("session-1").expect("id");
        let other = SessionId::parse("session-2").expect("id");
        let (guard, token) = registry.reserve(&session).expect("first run");
        assert!(registry.reserve(&session).is_none());
        assert!(registry.reserve(&other).is_some());
        assert!(!token.is_cancelled());
        drop(guard);
        assert!(registry.reserve(&session).is_some());
    }

    #[test]
    fn run_registry_cancel_all_cancels_every_run() {
        let registry = RunRegistry::new();
        let session = SessionId::parse("session-1").expect("id");
        let other = SessionId::parse("session-2").expect("id");
        let (guard, token) = registry.reserve(&session).expect("first run");
        let (other_guard, other_token) = registry.reserve(&other).expect("second run");

        registry.cancel_all();
        assert!(token.is_cancelled());
        assert!(other_token.is_cancelled());
        assert!(registry.reserve(&session).is_some());

        drop(guard);
        drop(other_guard);
    }

    #[test]
    fn a_stale_guard_does_not_free_a_newer_reservation() {
        let registry = RunRegistry::new();
        let session = SessionId::parse("session-1").expect("id");
        let (stale, _token) = registry.reserve(&session).expect("first run");
        registry.cancel_all();
        let (current, _token) = registry.reserve(&session).expect("second run");
        drop(stale);
        assert!(registry.reserve(&session).is_none());
        drop(current);
        assert!(registry.reserve(&session).is_some());
    }

    #[test]
    fn failure_codes_are_stable() {
        let cases = [
            (
                AgentError::Store(StoreError::Backend("boom".to_string())),
                "STORE_ERROR",
            ),
            (
                AgentError::InvalidProviderName("bad name".to_string()),
                "INVALID_PROVIDER",
            ),
            (AgentError::MissingFinish, "AGENT_ERROR"),
            (AgentError::EmptyResponse, "AGENT_ERROR"),
            (
                AgentError::MalformedStream("garbled".to_string()),
                "AGENT_ERROR",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(failure_code(&error), expected);
            assert_ne!(error.to_string(), "");
        }
    }

    #[tokio::test]
    async fn serve_stops_when_the_cancel_token_fires() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            serve(state, SocketAddr::from(([127, 0, 0, 1], 0)), cancel),
        )
        .await
        .expect("serve must return after cancellation");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn serve_reports_a_bind_failure() {
        let occupied = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = occupied.local_addr().expect("address");
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(text_provider("hi")),
        );
        let error = serve(state, address, CancellationToken::new())
            .await
            .expect_err("address is already bound");
        assert!(matches!(error, WebError::Bind { addr, .. } if addr == address));
    }
}
