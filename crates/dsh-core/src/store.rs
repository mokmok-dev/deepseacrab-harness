//! The durable session store seam.

use crate::event::{EventRecord, SessionEvent};
use crate::ids::SessionId;
use crate::session::{SessionHeader, SessionSummary};
use thiserror::Error;

/// Storage failures reported by a session backend.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// A session with this id already exists.
    #[error("session {0} already exists")]
    AlreadyExists(SessionId),
    /// No session with this id exists.
    #[error("session {0} not found")]
    NotFound(SessionId),
    /// The stored session uses a format this build cannot read.
    #[error("session {0} uses unsupported format version {1}")]
    UnsupportedVersion(SessionId, u32),
    /// The backend failed.
    #[error("storage failure: {0}")]
    Backend(String),
    /// An event could not be encoded or decoded.
    #[error("session event encoding failure: {0}")]
    Encoding(String),
}

/// Append-only session persistence.
///
/// Implementations assign consecutive positions per session and return the
/// record they committed. `create` also commits the opening `session_created`
/// fact so every readable session has a non-empty log.
pub trait SessionStore: Send + Sync {
    /// Create a session and its opening fact.
    fn create(&self, header: &SessionHeader) -> Result<(), StoreError>;

    /// Append one fact at `at` and return the committed record.
    fn append(
        &self,
        session_id: &SessionId,
        at: i64,
        event: SessionEvent,
    ) -> Result<EventRecord, StoreError>;

    /// Load every fact in position order.
    fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError>;

    /// List sessions without loading their logs.
    fn list(&self) -> Result<Vec<SessionSummary>, StoreError>;

    /// Report whether a session exists.
    fn has(&self, session_id: &SessionId) -> Result<bool, StoreError>;

    /// Delete a session and its log. Returns whether anything was removed.
    fn delete(&self, session_id: &SessionId) -> Result<bool, StoreError>;
}
