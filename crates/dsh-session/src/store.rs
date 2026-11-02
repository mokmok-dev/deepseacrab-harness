use dsh_core::event::{EventRecord, SessionEvent, TurnOutcome};
use dsh_core::ids::{EventSeq, SessionId};
use dsh_core::session::{SESSION_FORMAT_VERSION, SessionHeader, SessionSearchHit, SessionSummary};
use dsh_core::store::{SessionStore, StoreError};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    cwd TEXT,
    parent_session TEXT,
    title TEXT,
    format_version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (session_id, seq),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);";

/// `SQLite`-backed session persistence.
pub struct SqliteSessionStore {
    connection: Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Open a database file, creating missing parent directories first.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(backend)?;
        }
        let connection = Connection::open(path).map_err(backend)?;
        connection
            .execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(backend)?;
        Self::initialized(connection)
    }

    /// Open a private in-memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::initialized(Connection::open_in_memory().map_err(backend)?)
    }

    fn initialized(connection: Connection) -> Result<Self, StoreError> {
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(backend)?;
        connection.execute_batch(SCHEMA).map_err(backend)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::Backend("session store lock is poisoned".to_string()))
    }
}

impl SessionStore for SqliteSessionStore {
    fn create(&self, header: &SessionHeader) -> Result<(), StoreError> {
        if !header.is_current() {
            return Err(StoreError::UnsupportedVersion(
                header.id.clone(),
                header.format_version,
            ));
        }
        let connection = self.lock()?;
        create_session(&connection, header)
    }

    fn append(
        &self,
        session_id: &SessionId,
        at: i64,
        event: SessionEvent,
    ) -> Result<EventRecord, StoreError> {
        let connection = self.lock()?;
        append_event(&connection, session_id, at, event)
    }

    fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
        let connection = self.lock()?;
        load_events(&connection, session_id)
    }

    fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let connection = self.lock()?;
        list_sessions(&connection)
    }

    fn has(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let connection = self.lock()?;
        session_exists(&connection, session_id)
    }

    fn delete(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let connection = self.lock()?;
        delete_session(&connection, session_id)
    }

    fn header(&self, session_id: &SessionId) -> Result<SessionHeader, StoreError> {
        let connection = self.lock()?;
        read_header(&connection, session_id)
    }

    fn fork(
        &self,
        source: &SessionId,
        target: &SessionHeader,
        through: EventSeq,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        fork_session(&connection, source, target, through)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SessionSearchHit>, StoreError> {
        let connection = self.lock()?;
        search_events(&connection, query, limit)
    }

    fn repair(&self, session_id: &SessionId, at: i64) -> Result<bool, StoreError> {
        let connection = self.lock()?;
        repair_session(&connection, session_id, at)
    }
}

fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

fn create_session(connection: &Connection, header: &SessionHeader) -> Result<(), StoreError> {
    let transaction = connection.unchecked_transaction().map_err(backend)?;
    transaction
        .execute(
            "INSERT INTO sessions (id, created_at, cwd, parent_session, title, format_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                header.id.as_str(),
                header.created_at,
                header.cwd,
                header.parent_session.as_ref().map(SessionId::as_str),
                header.title,
                i64::from(header.format_version),
            ],
        )
        .map_err(|error| {
            if is_duplicate(&error) {
                StoreError::AlreadyExists(header.id.clone())
            } else {
                backend(error)
            }
        })?;
    insert_event(
        &transaction,
        &header.id,
        0,
        header.created_at,
        &SessionEvent::SessionCreated {
            cwd: header.cwd.clone(),
            created_at: header.created_at,
            format_version: header.format_version,
        },
    )?;
    transaction.commit().map_err(backend)
}

fn append_event(
    connection: &Connection,
    session_id: &SessionId,
    at: i64,
    event: SessionEvent,
) -> Result<EventRecord, StoreError> {
    let transaction = connection.unchecked_transaction().map_err(backend)?;
    let version: Option<u32> = transaction
        .query_row(
            "SELECT format_version FROM sessions WHERE id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    let Some(version) = version else {
        return Err(StoreError::NotFound(session_id.clone()));
    };
    if version != SESSION_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion(session_id.clone(), version));
    }
    let next: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM events WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .map_err(backend)?;
    let seq = sequence(next)?;
    insert_event(&transaction, session_id, next, at, &event)?;
    transaction.commit().map_err(backend)?;
    Ok(EventRecord { seq, at, event })
}

fn insert_event(
    connection: &Connection,
    session_id: &SessionId,
    seq: i64,
    at: i64,
    event: &SessionEvent,
) -> Result<(), StoreError> {
    let payload =
        serde_json::to_string(event).map_err(|error| StoreError::Encoding(error.to_string()))?;
    connection
        .execute(
            "INSERT INTO events (session_id, seq, at, kind, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id.as_str(), seq, at, event.kind(), payload],
        )
        .map_err(backend)?;
    Ok(())
}

fn load_events(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Vec<EventRecord>, StoreError> {
    if !session_exists(connection, session_id)? {
        return Err(StoreError::NotFound(session_id.clone()));
    }
    let mut statement = connection
        .prepare("SELECT seq, at, payload FROM events WHERE session_id = ?1 ORDER BY seq ASC")
        .map_err(backend)?;
    let rows = statement
        .query_map([session_id.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(backend)?;
    let mut records = Vec::new();
    for row in rows {
        let (seq, at, payload) = row.map_err(backend)?;
        let event = serde_json::from_str(&payload)
            .map_err(|error| StoreError::Encoding(error.to_string()))?;
        records.push(EventRecord {
            seq: sequence(seq)?,
            at,
            event,
        });
    }
    Ok(records)
}

fn list_sessions(connection: &Connection) -> Result<Vec<SessionSummary>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT sessions.id, sessions.created_at, sessions.cwd, sessions.title, \
             (SELECT COUNT(*) FROM events WHERE events.session_id = sessions.id) \
             FROM sessions ORDER BY sessions.created_at DESC, sessions.id ASC",
        )
        .map_err(backend)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(backend)?;
    let mut summaries = Vec::new();
    for row in rows {
        let (id, created_at, cwd, title, event_count) = row.map_err(backend)?;
        summaries.push(SessionSummary {
            id: SessionId::parse(id).map_err(backend)?,
            created_at,
            cwd,
            title,
            event_count: u64::try_from(event_count)
                .map_err(|_| StoreError::Backend("stored event count is negative".to_string()))?,
        });
    }
    Ok(summaries)
}

fn delete_session(connection: &Connection, session_id: &SessionId) -> Result<bool, StoreError> {
    let transaction = connection.unchecked_transaction().map_err(backend)?;
    let removed = transaction
        .execute("DELETE FROM sessions WHERE id = ?1", [session_id.as_str()])
        .map_err(backend)?;
    transaction.commit().map_err(backend)?;
    Ok(removed > 0)
}

fn session_exists(connection: &Connection, session_id: &SessionId) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .map_err(backend)
}

fn read_header(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<SessionHeader, StoreError> {
    let row = connection
        .query_row(
            "SELECT created_at, cwd, parent_session, title, format_version \
             FROM sessions WHERE id = ?1",
            [session_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(backend)?;
    let Some((created_at, cwd, parent_session, title, format_version)) = row else {
        return Err(StoreError::NotFound(session_id.clone()));
    };
    let parent_session = parent_session
        .map(SessionId::parse)
        .transpose()
        .map_err(|error| StoreError::Encoding(error.to_string()))?;
    let format_version = u32::try_from(format_version)
        .map_err(|_| StoreError::Encoding("format version is out of range".to_string()))?;
    Ok(SessionHeader {
        id: session_id.clone(),
        created_at,
        cwd,
        parent_session,
        title,
        format_version,
    })
}

fn is_duplicate(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn sequence(raw: i64) -> Result<EventSeq, StoreError> {
    u64::try_from(raw)
        .map(EventSeq::new)
        .map_err(|_| StoreError::Backend(format!("stored event sequence {raw} is negative")))
}

fn fork_session(
    connection: &Connection,
    source: &SessionId,
    target: &SessionHeader,
    through: EventSeq,
) -> Result<(), StoreError> {
    if !target.is_current() {
        return Err(StoreError::UnsupportedVersion(
            target.id.clone(),
            target.format_version,
        ));
    }
    let through = i64::try_from(through.get())
        .map_err(|_| StoreError::Backend("fork sequence is out of range".to_string()))?;
    let transaction = connection.unchecked_transaction().map_err(backend)?;
    let version: Option<u32> = transaction
        .query_row(
            "SELECT format_version FROM sessions WHERE id = ?1",
            [source.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    let Some(version) = version else {
        return Err(StoreError::NotFound(source.clone()));
    };
    if version != SESSION_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion(source.clone(), version));
    }
    let mut header = target.clone();
    header.parent_session = Some(source.clone());
    transaction
        .execute(
            "INSERT INTO sessions (id, created_at, cwd, parent_session, title, format_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                header.id.as_str(),
                header.created_at,
                header.cwd,
                header.parent_session.as_ref().map(SessionId::as_str),
                header.title,
                i64::from(header.format_version),
            ],
        )
        .map_err(|error| {
            if is_duplicate(&error) {
                StoreError::AlreadyExists(header.id.clone())
            } else {
                backend(error)
            }
        })?;
    insert_event(
        &transaction,
        &header.id,
        0,
        header.created_at,
        &SessionEvent::SessionCreated {
            cwd: header.cwd.clone(),
            created_at: header.created_at,
            format_version: header.format_version,
        },
    )?;
    {
        let mut statement = transaction
            .prepare(
                "SELECT at, payload FROM events \
                 WHERE session_id = ?1 AND seq > 0 AND seq <= ?2 ORDER BY seq ASC",
            )
            .map_err(backend)?;
        let rows = statement
            .query_map(params![source.as_str(), through], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(backend)?;
        let mut next = 1i64;
        for row in rows {
            let (at, payload) = row.map_err(backend)?;
            let event: SessionEvent = serde_json::from_str(&payload)
                .map_err(|error| StoreError::Encoding(error.to_string()))?;
            insert_event(&transaction, &header.id, next, at, &event)?;
            next = next
                .checked_add(1)
                .ok_or_else(|| StoreError::Backend("fork sequence overflow".to_string()))?;
        }
    }
    transaction.commit().map_err(backend)
}

fn search_events(
    connection: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionSearchHit>, StoreError> {
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let limit = i64::try_from(limit)
        .map_err(|_| StoreError::Backend("search limit is out of range".to_string()))?;
    let mut statement = connection
        .prepare(
            "SELECT session_id, seq, kind, payload FROM events \
             WHERE payload LIKE ?1 ESCAPE '\\' ORDER BY session_id ASC, seq ASC LIMIT ?2",
        )
        .map_err(backend)?;
    let rows = statement
        .query_map(params![pattern, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(backend)?;
    let mut hits = Vec::new();
    for row in rows {
        let (session_id, seq, kind, payload) = row.map_err(backend)?;
        let session_id = SessionId::parse(session_id)
            .map_err(|error| StoreError::Encoding(error.to_string()))?;
        hits.push(SessionSearchHit {
            session_id,
            seq: sequence(seq)?,
            kind,
            snippet: payload.chars().take(200).collect(),
        });
    }
    Ok(hits)
}

fn repair_session(
    connection: &Connection,
    session_id: &SessionId,
    at: i64,
) -> Result<bool, StoreError> {
    let events = load_events(connection, session_id)?;
    let Some(index) = events
        .iter()
        .rposition(|record| matches!(record.event, SessionEvent::TurnStarted { .. }))
    else {
        return Ok(false);
    };
    let SessionEvent::TurnStarted { turn_id } = &events[index].event else {
        return Ok(false);
    };
    let ended = events.iter().skip(index).any(|record| {
        matches!(
            &record.event,
            SessionEvent::TurnEnded { turn_id: ended, .. } if ended == turn_id
        )
    });
    if ended {
        return Ok(false);
    }
    let transaction = connection.unchecked_transaction().map_err(backend)?;
    let next: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM events WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .map_err(backend)?;
    insert_event(
        &transaction,
        session_id,
        next,
        at,
        &SessionEvent::TurnEnded {
            turn_id: turn_id.clone(),
            outcome: TurnOutcome::Failed {
                code: "INTERRUPTED".to_string(),
                message: "the process ended before the turn settled".to_string(),
            },
        },
    )?;
    transaction.commit().map_err(backend)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::derive_messages;
    use dsh_core::event::TurnOutcome;
    use dsh_core::ids::{MessageId, ModelId, ProviderId, StepId, ToolCallId, TurnId};
    use dsh_core::message::{
        AssistantMessage, ContentBlock, TokenUsage, ToolCall, ToolMessage, UserMessage,
    };
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn in_memory() -> SqliteSessionStore {
        SqliteSessionStore::open_in_memory().expect("in-memory store")
    }

    fn new_header() -> SessionHeader {
        SessionHeader::new(
            SessionId::generate(),
            1_700_000_000_000,
            Some("/work".to_string()),
        )
    }

    fn named_header(id: &str, created_at: i64) -> SessionHeader {
        SessionHeader::new(SessionId::parse(id).expect("valid id"), created_at, None)
    }

    fn opening(header: &SessionHeader) -> EventRecord {
        EventRecord {
            seq: EventSeq::FIRST,
            at: header.created_at,
            event: SessionEvent::SessionCreated {
                cwd: header.cwd.clone(),
                created_at: header.created_at,
                format_version: header.format_version,
            },
        }
    }

    fn summary(header: &SessionHeader, event_count: u64) -> SessionSummary {
        SessionSummary {
            id: header.id.clone(),
            created_at: header.created_at,
            cwd: header.cwd.clone(),
            title: header.title.clone(),
            event_count,
        }
    }

    fn user_message(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            message: UserMessage::text(text),
        }
    }

    fn temporary_database() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempdir().expect("temp dir");
        let path = directory.path().join("nested").join("sessions.db");
        (directory, path)
    }

    #[test]
    fn create_writes_the_opening_event_at_sequence_zero() {
        let store = in_memory();
        let mut header = new_header();
        header.title = Some("Trip plan".to_string());
        header.parent_session = Some(SessionId::generate());

        store.create(&header).expect("create");

        assert_eq!(
            store.load(&header.id).expect("load"),
            vec![opening(&header)]
        );
    }

    #[test]
    fn create_rejects_duplicate_ids_without_touching_the_log() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("first create");

        assert_eq!(
            store.create(&header),
            Err(StoreError::AlreadyExists(header.id.clone()))
        );
        assert_eq!(
            store.load(&header.id).expect("load"),
            vec![opening(&header)]
        );
    }

    #[test]
    fn create_reports_storage_failures_as_backend_errors() {
        let (_directory, path) = temporary_database();
        let store = SqliteSessionStore::open(&path).expect("open");
        {
            let raw = Connection::open(&path).expect("raw connection");
            raw.execute_batch("DROP TABLE events; DROP TABLE sessions;")
                .expect("drop schema");
        }

        assert!(matches!(
            store.create(&new_header()),
            Err(StoreError::Backend(_))
        ));
    }

    #[test]
    fn create_rejects_future_format_versions() {
        let store = in_memory();
        let mut header = new_header();
        header.format_version = SESSION_FORMAT_VERSION + 1;

        assert_eq!(
            store.create(&header),
            Err(StoreError::UnsupportedVersion(
                header.id.clone(),
                SESSION_FORMAT_VERSION + 1
            ))
        );
        assert!(!store.has(&header.id).expect("has"));
        assert_eq!(store.list().expect("list"), Vec::new());
    }

    #[test]
    fn append_returns_consecutive_positions() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("create");

        let first = store
            .append(&header.id, 10, user_message("one"))
            .expect("first append");
        let second = store
            .append(&header.id, 20, user_message("two"))
            .expect("second append");

        assert_eq!(first.seq, EventSeq::new(1));
        assert_eq!(first.at, 10);
        assert_eq!(second.seq, EventSeq::new(2));
        assert_eq!(second.at, 20);
    }

    #[test]
    fn load_returns_records_in_append_order() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("create");
        let events: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(user_message)
            .collect();
        for (index, event) in events.iter().cloned().enumerate() {
            let at = i64::try_from(index).expect("fits");
            store.append(&header.id, at, event).expect("append");
        }

        let loaded = store.load(&header.id).expect("load");

        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[0], opening(&header));
        for (index, event) in events.into_iter().enumerate() {
            let position = u64::try_from(index).expect("fits") + 1;
            assert_eq!(
                loaded[usize::try_from(position).expect("fits")].seq,
                EventSeq::new(position)
            );
            assert_eq!(
                loaded[usize::try_from(position).expect("fits")].event,
                event
            );
        }
    }

    #[test]
    fn load_of_an_unknown_session_is_not_found() {
        let store = in_memory();
        let session_id = SessionId::generate();

        assert_eq!(
            store.load(&session_id),
            Err(StoreError::NotFound(session_id))
        );
    }

    #[test]
    fn append_to_an_unknown_session_is_not_found() {
        let store = in_memory();
        let session_id = SessionId::generate();

        assert_eq!(
            store.append(&session_id, 0, user_message("lost")),
            Err(StoreError::NotFound(session_id))
        );
    }

    #[test]
    fn append_rejects_a_future_stored_version() {
        let (_directory, path) = temporary_database();
        let store = SqliteSessionStore::open(&path).expect("open");
        let header = new_header();
        store.create(&header).expect("create");
        {
            let raw = Connection::open(&path).expect("raw connection");
            raw.execute(
                "UPDATE sessions SET format_version = ?1 WHERE id = ?2",
                params![i64::from(SESSION_FORMAT_VERSION + 1), header.id.as_str()],
            )
            .expect("update version");
        }

        assert_eq!(
            store.append(&header.id, 1, user_message("late")),
            Err(StoreError::UnsupportedVersion(
                header.id.clone(),
                SESSION_FORMAT_VERSION + 1
            ))
        );
        assert_eq!(store.load(&header.id).expect("load").len(), 1);
    }

    #[test]
    fn list_returns_counts_ordered_by_creation_then_id() {
        let store = in_memory();
        let oldest = named_header("session-a", 100);
        let mut older = named_header("session-b", 300);
        older.cwd = Some("/work".to_string());
        older.title = Some("Beta".to_string());
        let middle = named_header("session-c", 200);
        let tie = named_header("session-d", 300);
        for header in [&oldest, &older, &middle, &tie] {
            store.create(header).expect("create");
        }
        for text in ["one", "two"] {
            store
                .append(&oldest.id, 1, user_message(text))
                .expect("append");
        }
        store
            .append(&older.id, 1, user_message("three"))
            .expect("append");

        assert_eq!(
            store.list().expect("list"),
            vec![
                summary(&older, 2),
                summary(&tie, 1),
                summary(&middle, 1),
                summary(&oldest, 3),
            ]
        );
    }

    #[test]
    fn has_and_delete_track_the_session_lifecycle() {
        let store = in_memory();
        let header = new_header();
        assert!(!store.has(&header.id).expect("has"));

        store.create(&header).expect("create");
        store
            .append(&header.id, 1, user_message("hello"))
            .expect("append");
        assert!(store.has(&header.id).expect("has"));

        assert!(store.delete(&header.id).expect("first delete"));
        assert!(!store.has(&header.id).expect("has"));
        assert_eq!(
            store.load(&header.id),
            Err(StoreError::NotFound(header.id.clone()))
        );
        assert!(!store.delete(&header.id).expect("second delete"));
        assert_eq!(store.list().expect("list"), Vec::new());
    }

    #[test]
    fn delete_cascades_through_the_event_log() {
        let (_directory, path) = temporary_database();
        let store = SqliteSessionStore::open(&path).expect("open");
        let header = new_header();
        store.create(&header).expect("create");
        store
            .append(&header.id, 1, user_message("hello"))
            .expect("append");

        store.delete(&header.id).expect("delete");

        let raw = Connection::open(&path).expect("raw connection");
        let remaining: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                [header.id.as_str()],
                |row| row.get(0),
            )
            .expect("count events");
        assert_eq!(remaining, 0);
    }

    #[test]
    fn reopening_a_file_store_preserves_the_log() {
        let (_directory, path) = temporary_database();
        let header = new_header();
        let events = [user_message("first"), user_message("second")];
        {
            let store = SqliteSessionStore::open(&path).expect("open");
            store.create(&header).expect("create");
            for (index, event) in events.iter().cloned().enumerate() {
                let at = 10 + i64::try_from(index).expect("fits");
                store.append(&header.id, at, event).expect("append");
            }
        }

        let store = SqliteSessionStore::open(&path).expect("reopen");

        let loaded = store.load(&header.id).expect("load");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0], opening(&header));
        for (index, event) in events.into_iter().enumerate() {
            let position = index + 1;
            assert_eq!(loaded[position].event, event);
            assert_eq!(
                loaded[position].at,
                10 + i64::try_from(index).expect("fits")
            );
        }
        assert_eq!(store.list().expect("list"), vec![summary(&header, 3)]);
        assert!(store.has(&header.id).expect("has"));
    }

    #[test]
    fn file_stores_use_wal() {
        let (_directory, path) = temporary_database();
        let _store = SqliteSessionStore::open(&path).expect("open");

        let raw = Connection::open(&path).expect("raw connection");
        let journal_mode: String = raw
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        assert_eq!(journal_mode, "wal");
    }

    #[test]
    fn stored_rows_keep_the_kind_and_parent_link() {
        let (_directory, path) = temporary_database();
        let store = SqliteSessionStore::open(&path).expect("open");
        let mut header = new_header();
        header.parent_session = Some(SessionId::generate());
        store.create(&header).expect("create");
        store
            .append(&header.id, 1, user_message("hello"))
            .expect("append");

        let raw = Connection::open(&path).expect("raw connection");
        let parent: Option<String> = raw
            .query_row(
                "SELECT parent_session FROM sessions WHERE id = ?1",
                [header.id.as_str()],
                |row| row.get(0),
            )
            .expect("parent");
        assert_eq!(
            parent.as_deref(),
            header.parent_session.as_ref().map(SessionId::as_str)
        );
        let kinds: Vec<String> = raw
            .prepare("SELECT kind FROM events WHERE session_id = ?1 ORDER BY seq")
            .expect("prepare")
            .query_map([header.id.as_str()], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("kinds");
        assert_eq!(kinds, ["session_created", "user_message"]);
    }

    #[test]
    fn corrupt_payloads_are_reported_as_encoding_failures() {
        let (_directory, path) = temporary_database();
        let store = SqliteSessionStore::open(&path).expect("open");
        let header = new_header();
        store.create(&header).expect("create");
        {
            let raw = Connection::open(&path).expect("raw connection");
            raw.execute(
                "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND seq = 0",
                params!["{", header.id.as_str()],
            )
            .expect("corrupt payload");
        }

        assert!(matches!(
            store.load(&header.id),
            Err(StoreError::Encoding(_))
        ));
    }

    #[test]
    fn concurrent_appends_receive_unique_consecutive_positions() {
        const THREADS: usize = 4;
        const APPENDS: usize = 25;

        let store = Arc::new(in_memory());
        let header = new_header();
        store.create(&header).expect("create");
        let mut handles = Vec::new();
        for thread_index in 0..THREADS {
            let store = Arc::clone(&store);
            let session_id = header.id.clone();
            handles.push(thread::spawn(move || {
                for event_index in 0..APPENDS {
                    let at = i64::try_from(thread_index * APPENDS + event_index).expect("fits");
                    store
                        .append(
                            &session_id,
                            at,
                            user_message(&format!("{thread_index}-{event_index}")),
                        )
                        .expect("append");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("thread joins");
        }

        let loaded = store.load(&header.id).expect("load");
        assert_eq!(loaded.len(), THREADS * APPENDS + 1);
        let sequences: Vec<u64> = loaded.iter().map(|record| record.seq.get()).collect();
        assert_eq!(
            sequences,
            (0..=u64::try_from(THREADS * APPENDS).expect("fits")).collect::<Vec<_>>()
        );
    }

    fn identifier() -> impl Strategy<Value = String> {
        "[A-Za-z0-9_-]{1,32}".prop_map(String::from)
    }

    fn content_block() -> impl Strategy<Value = ContentBlock> {
        prop_oneof![
            any::<String>().prop_map(ContentBlock::text),
            any::<String>().prop_map(ContentBlock::reasoning),
            (identifier(), "[a-z_]{1,16}", any::<String>()).prop_map(|(id, name, arguments)| {
                ContentBlock::tool_call(ToolCall {
                    id: ToolCallId::parse(id).expect("valid call id"),
                    name,
                    arguments,
                })
            }),
        ]
    }

    fn assistant_message() -> impl Strategy<Value = AssistantMessage> {
        (
            prop::collection::vec(content_block(), 0..4),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
        )
            .prop_map(
                |(content, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens)| {
                    AssistantMessage::new(
                        content,
                        ProviderId::parse("deepseek").expect("provider"),
                        ModelId::parse("deepseek-flash").expect("model"),
                        TokenUsage {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                            cache_write_tokens,
                        },
                    )
                },
            )
    }

    fn outcome() -> impl Strategy<Value = TurnOutcome> {
        prop_oneof![
            Just(TurnOutcome::Completed),
            any::<String>().prop_map(|reason| TurnOutcome::Cancelled { reason }),
            (any::<String>(), any::<String>())
                .prop_map(|(code, message)| TurnOutcome::Failed { code, message }),
            Just(TurnOutcome::StepLimit),
        ]
    }

    fn event() -> impl Strategy<Value = SessionEvent> {
        prop_oneof![
            (any::<Option<String>>(), any::<i64>(), 0u32..=2).prop_map(
                |(cwd, created_at, format_version)| SessionEvent::SessionCreated {
                    cwd,
                    created_at,
                    format_version,
                }
            ),
            identifier().prop_map(|turn_id| SessionEvent::TurnStarted {
                turn_id: TurnId::parse(turn_id).expect("valid turn id"),
            }),
            (identifier(), identifier()).prop_map(|(turn_id, step_id)| {
                SessionEvent::StepStarted {
                    turn_id: TurnId::parse(turn_id).expect("valid turn id"),
                    step_id: StepId::parse(step_id).expect("valid step id"),
                }
            }),
            any::<String>().prop_map(|text| SessionEvent::UserMessage {
                message: UserMessage::text(text),
            }),
            assistant_message().prop_map(|message| SessionEvent::AssistantMessage { message }),
            (identifier(), any::<String>()).prop_map(|(call_id, text)| {
                SessionEvent::ToolResult {
                    message: ToolMessage::text(
                        ToolCallId::parse(call_id).expect("valid call id"),
                        text,
                    ),
                }
            }),
            (identifier(), outcome()).prop_map(|(turn_id, outcome)| SessionEvent::TurnEnded {
                turn_id: TurnId::parse(turn_id).expect("valid turn id"),
                outcome,
            }),
        ]
    }

    proptest! {
        #[test]
        fn append_and_load_round_trip_every_event(
            events in prop::collection::vec(event(), 0..24)
        ) {
            let store = in_memory();
            let header = new_header();
            store.create(&header).expect("create");
            let mut expected = vec![opening(&header)];
            for (index, event) in events.iter().cloned().enumerate() {
                let at = i64::try_from(index).expect("fits");
                expected.push(store.append(&header.id, at, event).expect("append"));
            }

            let loaded = store.load(&header.id).expect("load");

            prop_assert_eq!(&loaded, &expected);
            let sequences: Vec<u64> = loaded.iter().map(|record| record.seq.get()).collect();
            let expected_sequences: Vec<u64> =
                (0..=u64::try_from(events.len()).expect("fits")).collect();
            prop_assert_eq!(sequences, expected_sequences);
        }

        #[test]
        fn loaded_log_derives_the_same_messages(
            events in prop::collection::vec(event(), 0..24)
        ) {
            let store = in_memory();
            let header = new_header();
            store.create(&header).expect("create");
            let mut input = vec![opening(&header)];
            for (index, event) in events.into_iter().enumerate() {
                let at = i64::try_from(index).expect("fits");
                input.push(store.append(&header.id, at, event).expect("append"));
            }

            let loaded = store.load(&header.id).expect("load");

            prop_assert_eq!(derive_messages(&loaded), derive_messages(&input));
            prop_assert_eq!(loaded.iter().map(|record| &record.event).collect::<Vec<_>>(), input.iter().map(|record| &record.event).collect::<Vec<_>>());
        }
    }

    #[test]
    fn message_ids_survive_the_round_trip() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("create");
        let message = AssistantMessage::new(
            vec![ContentBlock::text("hello")],
            ProviderId::parse("deepseek").expect("provider"),
            ModelId::parse("deepseek-flash").expect("model"),
            TokenUsage::default(),
        );
        let message_id: MessageId = message.id.clone();
        let event = SessionEvent::AssistantMessage { message };

        let record = store.append(&header.id, 1, event).expect("append");

        assert_eq!(record.event.message().expect("message").id(), &message_id);
        assert_eq!(
            store.load(&header.id).expect("load")[1]
                .event
                .message()
                .expect("message")
                .id(),
            &message_id
        );
    }

    #[test]
    fn fork_copies_a_prefix_into_a_new_session() {
        let store = in_memory();
        let source = new_header();
        store.create(&source).expect("create");
        for text in ["one", "two", "three"] {
            store
                .append(&source.id, 10, user_message(text))
                .expect("append");
        }
        let target = SessionHeader::new(SessionId::generate(), 20, Some("/fork".to_string()));

        store
            .fork(&source.id, &target, EventSeq::new(2))
            .expect("fork");

        let copied = store.load(&target.id).expect("load target");
        assert_eq!(copied.len(), 3);
        assert_eq!(copied[0], opening(&target));
        assert_eq!(copied[1].event.message().expect("message").text(), "one");
        assert_eq!(copied[2].event.message().expect("message").text(), "two");
        assert_eq!(store.load(&source.id).expect("source").len(), 4);
    }

    #[test]
    fn fork_at_zero_copies_only_the_opening_fact() {
        let store = in_memory();
        let source = new_header();
        store.create(&source).expect("create");
        store
            .append(&source.id, 10, user_message("one"))
            .expect("append");
        let target = SessionHeader::new(SessionId::generate(), 20, None);

        store
            .fork(&source.id, &target, EventSeq::new(0))
            .expect("fork");

        let copied = store.load(&target.id).expect("load target");
        assert_eq!(copied, vec![opening(&target)]);
    }

    #[test]
    fn fork_rejects_missing_sources_and_duplicate_targets() {
        let store = in_memory();
        let missing = SessionId::generate();
        let target = new_header();
        assert_eq!(
            store.fork(&missing, &target, EventSeq::new(0)),
            Err(StoreError::NotFound(missing))
        );

        let source = new_header();
        store.create(&source).expect("create");
        store
            .fork(&source.id, &target, EventSeq::new(0))
            .expect("fork");
        assert_eq!(
            store.fork(&source.id, &target, EventSeq::new(0)),
            Err(StoreError::AlreadyExists(target.id.clone()))
        );
    }

    #[test]
    fn fork_rejects_a_future_target_format() {
        let store = in_memory();
        let source = new_header();
        store.create(&source).expect("create");
        let mut target = SessionHeader::new(SessionId::generate(), 0, None);
        target.format_version = SESSION_FORMAT_VERSION + 1;
        assert_eq!(
            store.fork(&source.id, &target, EventSeq::new(0)),
            Err(StoreError::UnsupportedVersion(
                target.id.clone(),
                SESSION_FORMAT_VERSION + 1
            ))
        );
    }

    #[test]
    fn export_produces_one_json_record_per_line() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("create");
        store
            .append(&header.id, 10, user_message("one"))
            .expect("append");
        store
            .append(&header.id, 20, user_message("two"))
            .expect("append");

        let exported = store.export(&header.id).expect("export");
        let lines: Vec<_> = exported.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            let record: EventRecord = serde_json::from_str(line).expect("record");
            assert_ne!(record.event.kind(), "");
        }
    }

    #[test]
    fn search_finds_payload_text_and_respects_limits() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("create");
        store
            .append(&header.id, 10, user_message("needle in a haystack"))
            .expect("append");
        store
            .append(&header.id, 20, user_message("something else"))
            .expect("append");

        let hits = store.search("needle", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, header.id);
        assert!(hits[0].snippet.contains("needle"));
        assert_eq!(store.search("absent", 10).expect("search"), Vec::new());
        assert_eq!(store.search("", 10).expect("search"), Vec::new());
        assert_eq!(store.search("needle", 0).expect("search"), Vec::new());
    }

    #[test]
    fn repair_seals_an_open_turn_once() {
        let store = in_memory();
        let header = new_header();
        store.create(&header).expect("create");
        let turn_id = TurnId::generate();
        store
            .append(
                &header.id,
                10,
                SessionEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
            )
            .expect("append");

        assert!(store.repair(&header.id, 99).expect("repair"));
        let events = store.load(&header.id).expect("load");
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[2].event,
            SessionEvent::TurnEnded {
                turn_id,
                outcome: TurnOutcome::Failed {
                    code: "INTERRUPTED".to_string(),
                    message: "the process ended before the turn settled".to_string(),
                },
            }
        );
        assert!(!store.repair(&header.id, 100).expect("second repair"));
    }

    #[test]
    fn repair_ignores_settled_or_empty_sessions() {
        let store = in_memory();
        let empty = new_header();
        store.create(&empty).expect("create");
        assert!(!store.repair(&empty.id, 0).expect("empty repair"));

        let settled = new_header();
        store.create(&settled).expect("create");
        let turn_id = TurnId::generate();
        store
            .append(
                &settled.id,
                1,
                SessionEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
            )
            .expect("start");
        store
            .append(
                &settled.id,
                2,
                SessionEvent::TurnEnded {
                    turn_id,
                    outcome: TurnOutcome::Completed,
                },
            )
            .expect("end");
        assert!(!store.repair(&settled.id, 3).expect("settled repair"));
    }

    #[test]
    fn repair_of_an_unknown_session_is_not_found() {
        let store = in_memory();
        let session_id = SessionId::generate();
        assert_eq!(
            store.repair(&session_id, 0),
            Err(StoreError::NotFound(session_id))
        );
    }

    #[test]
    fn headers_round_trip_with_lineage() {
        let store = in_memory();
        let parent = SessionId::generate();
        let mut header = new_header();
        header.parent_session = Some(parent);
        header.title = Some("title".to_string());
        store.create(&header).expect("create");
        assert_eq!(store.header(&header.id).expect("header"), header);

        let target = SessionHeader::new(SessionId::generate(), 2, Some("/fork".to_string()));
        store
            .fork(&header.id, &target, EventSeq::new(0))
            .expect("fork");
        let forked = store.header(&target.id).expect("forked header");
        assert_eq!(forked.parent_session, Some(header.id));
        assert_eq!(forked.cwd.as_deref(), Some("/fork"));
    }

    #[test]
    fn headers_of_unknown_sessions_are_not_found() {
        let store = in_memory();
        let session_id = SessionId::generate();
        assert_eq!(
            store.header(&session_id),
            Err(StoreError::NotFound(session_id))
        );
    }
}
