//! The durable session store seam.

use crate::event::{EventRecord, SessionEvent};
use crate::ids::{EventSeq, SessionId};
use crate::session::{SessionHeader, SessionSearchHit, SessionSummary};
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
    /// The backend does not implement this operation.
    #[error("unsupported session operation: {0}")]
    Unsupported(String),
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

    /// Read a session header.
    fn header(&self, _session_id: &SessionId) -> Result<SessionHeader, StoreError> {
        Err(StoreError::Unsupported("header".to_string()))
    }

    /// Copy a session prefix into a new session.
    fn fork(
        &self,
        _source: &SessionId,
        _target: &SessionHeader,
        _through: EventSeq,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("fork".to_string()))
    }

    /// Export a session as newline-delimited JSON.
    fn export(&self, session_id: &SessionId) -> Result<String, StoreError> {
        let records = self.load(session_id)?;
        let mut output = String::new();
        for record in records {
            let line = serde_json::to_string(&record).map_err(|error| {
                StoreError::Encoding(format!("cannot encode session event: {error}"))
            })?;
            output.push_str(&line);
            output.push('\n');
        }
        Ok(output)
    }

    /// Search durable event payloads for a substring.
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SessionSearchHit>, StoreError> {
        Err(StoreError::Unsupported("search".to_string()))
    }

    /// Seal a turn that a process loss left open. Returns whether a fact was appended.
    fn repair(&self, _session_id: &SessionId, _at: i64) -> Result<bool, StoreError> {
        Err(StoreError::Unsupported("repair".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventRecord, SessionEvent};
    use crate::ids::{EventSeq, SessionId};
    use crate::session::{SessionHeader, SessionSummary};

    struct ReadOnlyStore;

    impl SessionStore for ReadOnlyStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
            Ok(())
        }
        fn append(
            &self,
            _session_id: &SessionId,
            _at: i64,
            _event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            Err(StoreError::Backend("read-only".to_string()))
        }
        fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
            Ok(vec![EventRecord {
                seq: EventSeq::FIRST,
                at: 1,
                event: SessionEvent::SessionCreated {
                    cwd: None,
                    created_at: 1,
                    format_version: 1,
                },
            }])
        }
        fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
            Ok(Vec::new())
        }
        fn has(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(true)
        }
        fn delete(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    #[test]
    fn default_admin_operations_report_unsupported() {
        let store = ReadOnlyStore;
        let session_id = SessionId::generate();
        let header = SessionHeader::new(session_id.clone(), 1, None);
        assert_eq!(
            store.fork(&session_id, &header, EventSeq::FIRST),
            Err(StoreError::Unsupported("fork".to_string()))
        );
        assert_eq!(
            store.search("query", 10),
            Err(StoreError::Unsupported("search".to_string()))
        );
        assert_eq!(
            store.repair(&session_id, 1),
            Err(StoreError::Unsupported("repair".to_string()))
        );
        assert_eq!(
            store.header(&session_id),
            Err(StoreError::Unsupported("header".to_string()))
        );
    }

    #[test]
    fn default_export_serializes_the_loaded_log() {
        let store = ReadOnlyStore;
        let exported = store.export(&SessionId::generate()).expect("export");
        assert!(exported.ends_with('\n'));
        let record: EventRecord = serde_json::from_str(exported.trim_end()).expect("record");
        assert_eq!(record.seq, EventSeq::FIRST);
        assert_eq!(record.event.kind(), "session_created");
    }
}
