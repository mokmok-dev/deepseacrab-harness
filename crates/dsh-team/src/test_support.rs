//! Fixtures shared by the runtime and tool tests.

use async_trait::async_trait;
use dsh_agent::AgentConfig;
use dsh_core::{
    EventRecord, EventSeq, FinishReason, LlmError, LlmProvider, LlmRequest, LlmStream, ModelId,
    SessionEvent, SessionHeader, SessionId, SessionStore, SessionSummary, StoreError, StreamEvent,
    Tool, ToolContext, ToolOutcome, ToolSchema, ToolSession,
};
use futures_core::Stream;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct MockStore {
    sessions: Mutex<BTreeMap<SessionId, (SessionHeader, Vec<EventRecord>)>>,
}

#[allow(clippy::significant_drop_tightening)]
impl SessionStore for MockStore {
    fn create(&self, header: &SessionHeader) -> Result<(), StoreError> {
        let mut sessions = self.sessions.lock().expect("sessions");
        if sessions.contains_key(&header.id) {
            return Err(StoreError::AlreadyExists(header.id.clone()));
        }
        let opening = EventRecord {
            seq: EventSeq::FIRST,
            at: header.created_at,
            event: SessionEvent::SessionCreated {
                cwd: header.cwd.clone(),
                created_at: header.created_at,
                format_version: header.format_version,
            },
        };
        sessions.insert(header.id.clone(), (header.clone(), vec![opening]));
        Ok(())
    }

    fn append(
        &self,
        session_id: &SessionId,
        at: i64,
        event: SessionEvent,
    ) -> Result<EventRecord, StoreError> {
        let mut sessions = self.sessions.lock().expect("sessions");
        let Some((_, events)) = sessions.get_mut(session_id) else {
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
        let sessions = self.sessions.lock().expect("sessions");
        sessions
            .get(session_id)
            .map(|(_, events)| events.clone())
            .ok_or_else(|| StoreError::NotFound(session_id.clone()))
    }

    fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let sessions = self.sessions.lock().expect("sessions");
        Ok(sessions
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
            .sessions
            .lock()
            .expect("sessions")
            .contains_key(session_id))
    }

    fn delete(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        Ok(self
            .sessions
            .lock()
            .expect("sessions")
            .remove(session_id)
            .is_some())
    }

    fn header(&self, session_id: &SessionId) -> Result<SessionHeader, StoreError> {
        let sessions = self.sessions.lock().expect("sessions");
        sessions
            .get(session_id)
            .map(|(header, _)| header.clone())
            .ok_or_else(|| StoreError::NotFound(session_id.clone()))
    }
}

struct ScriptedStream {
    events: VecDeque<Result<StreamEvent, LlmError>>,
}

impl Stream for ScriptedStream {
    type Item = Result<StreamEvent, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.events.pop_front())
    }
}

pub struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn stream(
        &self,
        request: LlmRequest,
        _cancel: CancellationToken,
    ) -> Result<LlmStream, LlmError> {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .unwrap_or_default();
        Ok(Box::pin(ScriptedStream {
            events: events.into(),
        }))
    }
}

pub struct NamelessProvider;

#[async_trait]
impl LlmProvider for NamelessProvider {
    fn name(&self) -> &'static str {
        ""
    }

    async fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> Result<LlmStream, LlmError> {
        Err(LlmError::missing_credential("unused"))
    }
}

pub fn text_response(text: &str) -> Vec<Result<StreamEvent, LlmError>> {
    vec![
        Ok(StreamEvent::TextDelta {
            text: text.to_string(),
        }),
        Ok(StreamEvent::Finish {
            reason: FinishReason::Stop,
        }),
    ]
}

pub fn config(cwd: &Path) -> AgentConfig {
    AgentConfig::new(
        ModelId::parse("test-model").expect("model"),
        cwd.to_path_buf(),
    )
}

pub struct TestSession {
    pub(crate) id: SessionId,
}

impl ToolSession for TestSession {
    fn session_id(&self) -> &SessionId {
        &self.id
    }

    fn append(&self, _event: SessionEvent) -> Result<EventRecord, StoreError> {
        Err(StoreError::Unsupported("test session".to_string()))
    }

    fn now_millis(&self) -> i64 {
        0
    }
}

pub fn context(cwd: &Path, session: Option<SessionId>) -> ToolContext {
    let context = ToolContext::new(
        cwd.to_path_buf(),
        dsh_core::PermissionMode::WorkspaceWrite,
        CancellationToken::new(),
    );
    match session {
        Some(id) => context.with_session(Arc::new(TestSession { id })),
        None => context,
    }
}

pub struct NamedTool {
    pub(crate) name: &'static str,
}

#[async_trait]
impl Tool for NamedTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.to_string(),
            description: "test tool".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(&self, _context: &ToolContext, _arguments: serde_json::Value) -> ToolOutcome {
        ToolOutcome::text(self.name)
    }
}
