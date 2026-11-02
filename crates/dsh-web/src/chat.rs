//! The AG-UI streaming endpoint: one SSE body per run.

use crate::agui::{AguiEvent, EventBridge};
use crate::{ApiError, WebState, failure_code};
use axum::body::Bytes;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use dsh_agent::{AgentLoop, EventSink, RunEvent};
use dsh_core::{MAX_ID_LEN, SessionId};
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunAgentInput {
    thread_id: String,
    run_id: String,
    #[serde(default)]
    messages: Vec<WireMessage>,
}

impl RunAgentInput {
    fn parse(body: &[u8]) -> Result<Self, ApiError> {
        let input: Self = serde_json::from_slice(body)
            .map_err(|error| ApiError::BadRequest(format!("invalid request body: {error}")))?;
        if input.run_id.trim().is_empty()
            || input.run_id.len() > MAX_ID_LEN
            || input.run_id.chars().any(char::is_control)
        {
            return Err(ApiError::BadRequest(
                "runId must be a non-empty identifier without control characters".to_string(),
            ));
        }
        Ok(input)
    }
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: Option<WireContent>,
}

impl WireMessage {
    fn text(&self) -> String {
        self.content
            .as_ref()
            .map(WireContent::text)
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

impl WireContent {
    fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts.iter().filter_map(WirePart::text).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WirePart {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

impl WirePart {
    fn text(&self) -> Option<&str> {
        if self.kind == "text" {
            self.text.as_deref()
        } else {
            None
        }
    }
}

fn task_of(messages: &[WireMessage]) -> Option<String> {
    let message = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")?;
    let text = message.text();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[derive(Clone, Default)]
pub struct RunRegistry {
    runs: Arc<Mutex<HashMap<SessionId, Arc<CancellationToken>>>>,
}

impl RunRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim `session_id` for one run, or return nothing while another holds it.
    #[must_use]
    pub fn reserve(&self, session_id: &SessionId) -> Option<(RunGuard, CancellationToken)> {
        let token = Arc::new(CancellationToken::new());
        {
            let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            if runs.contains_key(session_id) {
                return None;
            }
            runs.insert(session_id.clone(), Arc::clone(&token));
        }
        let guard = RunGuard {
            runs: Arc::clone(&self.runs),
            session_id: session_id.clone(),
            token: Arc::clone(&token),
        };
        Some((guard, token.as_ref().clone()))
    }

    /// Cancel every live run and free every reservation.
    pub fn cancel_all(&self) {
        let tokens: Vec<_> = self
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, token)| token)
            .collect();
        for token in tokens {
            token.cancel();
        }
    }
}

pub struct RunGuard {
    runs: Arc<Mutex<HashMap<SessionId, Arc<CancellationToken>>>>,
    session_id: SessionId,
    token: Arc<CancellationToken>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let current = runs.get(&self.session_id);
        if current.is_some_and(|current| Arc::ptr_eq(current, &self.token)) {
            runs.remove(&self.session_id);
        }
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub async fn chat(State(state): State<WebState>, body: Bytes) -> Result<Response, ApiError> {
    let input = RunAgentInput::parse(&body)?;
    let thread_id = SessionId::parse(input.thread_id.clone())?;
    let Some(task) = task_of(&input.messages) else {
        return Err(ApiError::BadRequest(
            "messages must contain a non-blank user message".to_string(),
        ));
    };
    let Some((guard, cancel)) = state.runs.reserve(&thread_id) else {
        return Err(ApiError::Conflict(format!(
            "session {thread_id} already has a run"
        )));
    };

    let (sender, receiver) = mpsc::unbounded_channel();
    let bridge = Arc::new(Mutex::new(EventBridge::new(
        input.thread_id.clone(),
        input.run_id.clone(),
    )));
    let _ = sender.send(AguiEvent::RunStarted {
        thread_id: input.thread_id,
        run_id: input.run_id,
    });
    let sink: EventSink = {
        let bridge = Arc::clone(&bridge);
        let sender = sender.clone();
        Arc::new(move |event| dispatch(&bridge, &sender, event))
    };

    let WebState {
        store,
        provider,
        tools,
        config,
        clock,
        ..
    } = state;
    let stop = CancelOnDrop(cancel.clone());
    tokio::spawn(async move {
        let _guard = guard;
        let result = match AgentLoop::new(store, provider, tools, config, clock) {
            Ok(agent) => agent.run(&thread_id, &task, cancel, Some(sink)).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            dispatch(
                &bridge,
                &sender,
                RunEvent::Error {
                    code: failure_code(&error).to_string(),
                    message: error.to_string(),
                },
            );
        }
    });

    let frames = AguiStream {
        frames: UnboundedReceiverStream::new(receiver),
        _cancel: stop,
    };
    Ok(Sse::new(frames).into_response())
}

fn dispatch(
    bridge: &Mutex<EventBridge>,
    sender: &mpsc::UnboundedSender<AguiEvent>,
    event: RunEvent,
) {
    let events = bridge
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(event);
    for event in events {
        if sender.send(event).is_err() {
            break;
        }
    }
}

struct AguiStream {
    frames: UnboundedReceiverStream<AguiEvent>,
    _cancel: CancelOnDrop,
}

impl Stream for AguiStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Stream::poll_next(Pin::new(&mut this.frames), context).map(|frame| {
            frame.and_then(|event| {
                serde_json::to_string(&event)
                    .ok()
                    .map(|data| Ok(Event::default().data(data)))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn message(value: serde_json::Value) -> WireMessage {
        serde_json::from_value(value).expect("message")
    }

    #[test]
    fn task_uses_the_newest_user_message() {
        let messages = [
            message(serde_json::json!({"role": "user", "content": "old"})),
            message(serde_json::json!({"role": "assistant", "content": "reply"})),
            message(serde_json::json!({"role": "user", "content": "new"})),
        ];
        assert_eq!(task_of(&messages).as_deref(), Some("new"));
    }

    #[test]
    fn a_blank_newest_user_message_does_not_fall_back() {
        let messages = [
            message(serde_json::json!({"role": "user", "content": "old"})),
            message(serde_json::json!({"role": "user", "content": "  \n\t "})),
        ];
        assert_eq!(task_of(&messages), None);
    }

    #[test]
    fn text_parts_concatenate_in_order() {
        let messages = [message(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "tool-call", "toolCallId": "call-1"},
                {"type": "text", "text": "world"}
            ]
        }))];
        assert_eq!(task_of(&messages).as_deref(), Some("hello world"));
    }

    #[test]
    fn a_user_message_without_content_has_no_task() {
        let messages = [message(serde_json::json!({"role": "user"}))];
        assert_eq!(task_of(&messages), None);
    }

    #[test]
    fn messages_without_a_user_turn_have_no_task() {
        let messages = [
            message(serde_json::json!({"role": "assistant", "content": "hi"})),
            message(serde_json::json!({"role": "system", "content": "rules"})),
        ];
        assert_eq!(task_of(&messages), None);
        assert_eq!(task_of(&[]), None);
    }

    #[test]
    fn a_run_input_ignores_client_side_fields() {
        let input = RunAgentInput::parse(
            serde_json::json!({
                "threadId": "session-1",
                "runId": "run-1",
                "messages": [{"role": "user", "content": "hi"}],
                "state": {"any": "thing"},
                "tools": [{"name": "echo"}],
                "context": [{"description": "note"}],
                "forwardedProps": {"user": "u-1"}
            })
            .to_string()
            .as_bytes(),
        )
        .expect("input");
        assert_eq!(input.thread_id, "session-1");
        assert_eq!(input.messages.len(), 1);
    }

    #[test]
    fn an_invalid_run_id_is_rejected() {
        for run_id in [
            " ".to_string(),
            "a".repeat(MAX_ID_LEN + 1),
            "run\n-1".to_string(),
        ] {
            let body = serde_json::json!({
                "threadId": "session-1",
                "runId": run_id,
                "messages": [],
            })
            .to_string();
            let error = RunAgentInput::parse(body.as_bytes()).expect_err("invalid run id");
            assert!(matches!(error, ApiError::BadRequest(_)));
        }
    }

    proptest! {
        #[test]
        fn the_task_is_the_trimmed_newest_user_text(text in ".+") {
            let messages = [message(serde_json::json!({
                "role": "user",
                "content": text,
            }))];
            let trimmed = text.trim();
            let expected = (!trimmed.is_empty()).then(|| trimmed.to_string());
            prop_assert_eq!(task_of(&messages), expected);
        }
    }
}
