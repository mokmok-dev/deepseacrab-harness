//! Shared fixtures for tool tests.

use dsh_core::{
    EventRecord, EventSeq, PermissionMode, SessionEvent, SessionId, SessionSearchHit, StoreError,
    ToolContext, ToolSession,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[must_use]
pub fn context(directory: &Path, permission: PermissionMode) -> ToolContext {
    ToolContext::new(
        directory.to_path_buf(),
        permission,
        CancellationToken::new(),
    )
}

pub struct MockSession {
    id: SessionId,
    events: Mutex<Vec<SessionEvent>>,
    hits: Mutex<Vec<SessionSearchHit>>,
}

impl MockSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: SessionId::generate(),
            events: Mutex::new(Vec::new()),
            hits: Mutex::new(Vec::new()),
        }
    }

    pub fn set_hits(&self, hits: Vec<SessionSearchHit>) {
        *self.hits.lock().expect("hits") = hits;
    }

    #[must_use]
    pub fn events(&self) -> Vec<SessionEvent> {
        self.events.lock().expect("events").clone()
    }
}

impl Default for MockSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSession for MockSession {
    fn session_id(&self) -> &SessionId {
        &self.id
    }

    #[allow(clippy::significant_drop_tightening)]
    fn append(&self, event: SessionEvent) -> Result<EventRecord, StoreError> {
        let mut events = self.events.lock().expect("events");
        let seq = EventSeq::new(u64::try_from(events.len()).expect("length"));
        events.push(event.clone());
        Ok(EventRecord { seq, at: 1, event })
    }

    fn now_millis(&self) -> i64 {
        1
    }

    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SessionSearchHit>, StoreError> {
        Ok(self.hits.lock().expect("hits").clone())
    }
}

#[must_use]
pub fn session_context(
    directory: &Path,
    permission: PermissionMode,
) -> (ToolContext, Arc<MockSession>) {
    let session = Arc::new(MockSession::new());
    (
        context(directory, permission).with_session(session.clone()),
        session,
    )
}
