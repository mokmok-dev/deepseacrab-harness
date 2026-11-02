#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Browser client and JSON/SSE API for the headless harness.
//!
//! [`router`] is a pure function of [`WebState`], so the whole surface can be
//! driven with `tower::ServiceExt::oneshot` without binding a socket.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dsh_agent::{AgentConfig, AgentError, AgentLoop, EventSink, RunEvent};
use dsh_core::{
    Clock, EventRecord, IdError, LlmProvider, SessionId, SessionStore, SessionSummary, StoreError,
    ToolRegistry, ToolSchema,
};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

/// The chat page served at `/`.
const INDEX_HTML: &str = include_str!("../static/index.html");

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
enum ApiError {
    /// The request body or path did not match the contract.
    #[error("{0}")]
    BadRequest(String),
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

/// The wire body of `POST /api/run`.
#[derive(Deserialize)]
struct RunRequest {
    task: String,
    #[serde(default)]
    session_id: Option<String>,
}

/// A run request whose fields passed the boundary checks.
struct RunPlan {
    task: String,
    session_id: SessionId,
}

impl RunRequest {
    fn plan(body: &[u8]) -> Result<RunPlan, ApiError> {
        let request: Self = serde_json::from_slice(body)
            .map_err(|error| ApiError::BadRequest(format!("invalid request body: {error}")))?;
        if request.task.trim().is_empty() {
            return Err(ApiError::BadRequest("task must not be empty".to_string()));
        }
        let session_id = match request.session_id {
            Some(raw) => SessionId::parse(raw)?,
            None => SessionId::generate(),
        };
        Ok(RunPlan {
            task: request.task,
            session_id,
        })
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
        .route("/api/run", post(run_task))
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
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { cancel.cancelled().await })
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

async fn run_task(State(state): State<WebState>, body: Bytes) -> Result<Response, ApiError> {
    let plan = RunRequest::plan(&body)?;
    let (sender, receiver) = mpsc::unbounded_channel();
    let sink: EventSink = Arc::new(move |event| {
        let _ = sender.send(event);
    });
    tokio::spawn(async move {
        let result = match AgentLoop::new(
            state.store,
            state.provider,
            state.tools,
            state.config,
            state.clock,
        ) {
            Ok(agent) => {
                agent
                    .run(
                        &plan.session_id,
                        &plan.task,
                        CancellationToken::new(),
                        Some(Arc::clone(&sink)),
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            sink(failure_event(&error));
        }
    });
    let frames = UnboundedReceiverStream::new(receiver)
        .map(|event| serde_json::to_string(&event).map(|data| Event::default().data(data)));
    Ok(Sse::new(frames).into_response())
}

fn failure_event(error: &AgentError) -> RunEvent {
    let code = match error {
        AgentError::Store(_) => "STORE_ERROR",
        AgentError::InvalidProviderName(_) => "INVALID_PROVIDER",
        // The driver settles provider and stream failures into a turn itself,
        // so only a failure that skipped that path reaches this arm.
        _ => "AGENT_ERROR",
    };
    RunEvent::Error {
        code: code.to_string(),
        message: error.to_string(),
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
        SessionHeader, StreamEvent, Tool, ToolCallId, ToolContext, ToolOutcome,
    };
    use http_body_util::BodyExt;
    use proptest::prelude::*;
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
        for control in [
            "id=\"task\"",
            "id=\"session\"",
            "id=\"send\"",
            "id=\"transcript\"",
            "id=\"sessions\"",
        ] {
            assert!(html.contains(control), "page is missing {control}");
        }
        assert!(
            html.contains("fetch('/api/run'"),
            "page must post the run to the streaming endpoint"
        );
        assert!(
            html.contains("JSON.stringify({ task, session_id: sessionId || undefined })"),
            "page must send the run request shape"
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

    #[tokio::test]
    async fn run_rejects_invalid_requests() {
        for body in [
            r#"{"task":""}"#,
            r#"{"task":"  \n\t "}"#,
            r#"{"session_id":"session-known"}"#,
            "not json",
            r#"{"task":"hi","session_id":""}"#,
        ] {
            let state = web_state(
                Arc::new(MockStore::default()),
                Arc::new(text_provider("hi")),
            );
            let response = execute(state, post_request("/api/run", body)).await;
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
    async fn run_streams_text_tool_and_final_events() {
        let store = Arc::new(MockStore::default());
        let state = web_state(store.clone(), Arc::new(tool_then_text_provider()));
        let response = execute(
            state,
            post_request("/api/run", r#"{"task":"use the echo tool"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let frames = frames(&body_text(response).await);
        assert_eq!(
            event_types(&frames),
            ["session", "tool_call", "tool_result", "text", "final"]
        );
        assert_eq!(frames[1]["name"], "echo");
        assert_eq!(frames[1]["id"], "call-1");
        assert_eq!(frames[2]["is_error"], false);
        assert!(
            frames[2]["output"]
                .as_str()
                .is_some_and(|output| output.contains("hi"))
        );
        assert_eq!(frames[3]["text"], "done");
        assert_eq!(frames[4]["text"], "done");
        assert_eq!(frames[4]["outcome"]["outcome"], "completed");
        let session_id = frames[0]["session_id"].as_str().expect("session id");
        assert!(session_id.starts_with("session-"));
        let log = store
            .load(&SessionId::parse(session_id).expect("id"))
            .expect("log");
        assert!(
            log.iter()
                .any(|record| record.event.kind() == "user_message")
        );
        assert!(log.iter().any(|record| record.event.kind() == "turn_ended"));
    }

    #[tokio::test]
    async fn run_uses_the_requested_session() {
        let store = Arc::new(MockStore::default());
        let state = web_state(store.clone(), Arc::new(text_provider("hi")));
        let response = execute(
            state,
            post_request("/api/run", r#"{"task":"hi","session_id":"session-known"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&body_text(response).await);
        assert_eq!(frames[0]["session_id"], "session-known");
        let log = store
            .load(&SessionId::parse("session-known").expect("id"))
            .expect("log");
        assert!(
            log.iter()
                .any(|record| record.event.kind() == "user_message")
        );
    }

    #[tokio::test]
    async fn run_reports_setup_failures_as_an_error_event() {
        let state = web_state(
            Arc::new(MockStore::default()),
            Arc::new(ScriptedProvider::named("")),
        );
        let response = execute(state, post_request("/api/run", r#"{"task":"hi"}"#)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&body_text(response).await);
        assert_eq!(event_types(&frames), ["error"]);
        assert_eq!(frames[0]["code"], "INVALID_PROVIDER");
        assert!(
            frames[0]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
    }

    #[tokio::test]
    async fn run_reports_store_failures_as_an_error_event() {
        let state = web_state(Arc::new(BrokenStore), Arc::new(text_provider("hi")));
        let response = execute(state, post_request("/api/run", r#"{"task":"hi"}"#)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&body_text(response).await);
        assert_eq!(event_types(&frames), ["session", "error"]);
        assert_eq!(frames[1]["code"], "STORE_ERROR");
        assert!(
            frames[1]["message"]
                .as_str()
                .is_some_and(|message| message.contains("disk full"))
        );
    }

    #[test]
    fn failure_events_use_stable_codes() {
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
            let RunEvent::Error { code, message } = failure_event(&error) else {
                panic!("expected an error frame for {error}");
            };
            assert_eq!(code, expected);
            assert_ne!(message, "");
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

    proptest! {
        #[test]
        fn run_requests_accept_exactly_non_blank_tasks(task in ".*") {
            let body = json!({ "task": task }).to_string();
            match RunRequest::plan(body.as_bytes()) {
                Ok(plan) => {
                    prop_assert!(!task.trim().is_empty(), "accepted a blank task {:?}", task);
                    prop_assert_eq!(plan.task.as_str(), task.as_str());
                    prop_assert!(plan.session_id.as_str().starts_with("session-"));
                }
                Err(_) => prop_assert!(task.trim().is_empty(), "rejected a task {:?}", task),
            }
        }

        #[test]
        fn run_requests_honor_the_requested_session(raw in ".*") {
            let body = json!({ "task": "hi", "session_id": raw }).to_string();
            match RunRequest::plan(body.as_bytes()) {
                Ok(plan) => prop_assert_eq!(plan.session_id.as_str(), raw.as_str()),
                Err(_) => prop_assert!(SessionId::parse(raw).is_err()),
            }
        }
    }
}
