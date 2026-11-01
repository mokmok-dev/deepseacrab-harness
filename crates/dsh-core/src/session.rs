//! Session identity and storage metadata.

use crate::ids::SessionId;
use serde::{Deserialize, Serialize};

/// Logical format version stamped into newly written sessions.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Immutable storage metadata kept outside the conversation log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Session identity.
    pub id: SessionId,
    /// Unix epoch milliseconds when the session was created.
    pub created_at: i64,
    /// Working directory recorded at creation.
    pub cwd: Option<String>,
    /// Direct creator of a subagent session.
    pub parent_session: Option<SessionId>,
    /// Human-readable title, when one was set.
    pub title: Option<String>,
    /// Logical format version.
    pub format_version: u32,
}

impl SessionHeader {
    /// Build a header for a fresh top-level session.
    #[must_use]
    pub const fn new(id: SessionId, created_at: i64, cwd: Option<String>) -> Self {
        Self {
            id,
            created_at,
            cwd,
            parent_session: None,
            title: None,
            format_version: SESSION_FORMAT_VERSION,
        }
    }

    /// Whether this header uses the only format the store currently accepts.
    #[must_use]
    pub const fn is_current(&self) -> bool {
        self.format_version == SESSION_FORMAT_VERSION
    }
}

/// Listing metadata returned without loading the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session identity.
    pub id: SessionId,
    /// Unix epoch milliseconds when the session was created.
    pub created_at: i64,
    /// Working directory recorded at creation.
    pub cwd: Option<String>,
    /// Human-readable title, when one was set.
    pub title: Option<String>,
    /// Number of durable events.
    pub event_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_headers_are_current() {
        let header = SessionHeader::new(SessionId::generate(), 42, Some("/work".to_string()));
        assert!(header.is_current());
        assert_eq!(header.format_version, SESSION_FORMAT_VERSION);
        assert_eq!(header.created_at, 42);
        assert_eq!(header.cwd.as_deref(), Some("/work"));
        assert_eq!(header.parent_session, None);
        assert_eq!(header.title, None);
    }

    #[test]
    fn future_headers_are_not_current() {
        let mut header = SessionHeader::new(SessionId::generate(), 0, None);
        header.format_version = SESSION_FORMAT_VERSION + 1;
        assert!(!header.is_current());
    }
}
