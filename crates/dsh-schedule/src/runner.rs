//! The due-run driver: one agent turn per schedule whose time has come.

use crate::file::{ScheduleError, ScheduleFile};
use dsh_agent::{AgentConfig, AgentLoop, RunOutcome};
use dsh_core::{Clock, LlmProvider, SessionId, SessionStore, ToolRegistry};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// One settled scheduled turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleRun {
    /// Schedule that fired.
    pub schedule_id: String,
    /// Session the turn appended to.
    pub session_id: SessionId,
    /// How the turn settled.
    pub outcome: RunOutcome,
}

/// Run every enabled schedule that is due at the clock's current time.
///
/// Each schedule is advanced and saved as soon as its turn settles, so the next
/// due check does not repeat a turn this batch already ran. A failed turn is
/// returned as a [`ScheduleRun`]; only schedule-file, setup, and storage
/// failures stop the batch.
pub async fn run_due(
    file: &ScheduleFile,
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AgentConfig,
    clock: Arc<dyn Clock>,
) -> Result<Vec<ScheduleRun>, ScheduleError> {
    let now = clock.now_millis();
    let due = file.due(now)?;
    let mut runs = Vec::new();
    for schedule in due {
        let session_id = schedule
            .session_id
            .clone()
            .unwrap_or_else(SessionId::generate);
        let mut run_config = config.clone();
        run_config.cwd = PathBuf::from(&schedule.cwd);
        let agent = AgentLoop::new(
            Arc::clone(&store),
            Arc::clone(&provider),
            tools.clone(),
            run_config,
            Arc::clone(&clock),
        )?;
        let outcome = agent
            .run(&session_id, &schedule.task, CancellationToken::new(), None)
            .await?;
        file.advance(&schedule.id, now)?;
        runs.push(ScheduleRun {
            schedule_id: schedule.id,
            session_id,
            outcome,
        });
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_core::{
        EventRecord, EventSeq, FinishReason, FixedClock, LlmError, LlmRequest, LlmStream, ModelId,
        SessionEvent, SessionHeader, SessionSummary, StoreError, StreamEvent,
    };
    use futures_core::Stream;
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct MemoryStore {
        sessions: Mutex<BTreeMap<SessionId, (SessionHeader, Vec<EventRecord>)>>,
    }

    #[allow(clippy::significant_drop_tightening)]
    impl SessionStore for MemoryStore {
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

    struct BrokenStore;

    impl SessionStore for BrokenStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
            Err(StoreError::Backend("broken".to_string()))
        }

        fn append(
            &self,
            _session_id: &SessionId,
            _at: i64,
            _event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            Err(StoreError::Backend("broken".to_string()))
        }

        fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
            Err(StoreError::Backend("broken".to_string()))
        }

        fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
            Err(StoreError::Backend("broken".to_string()))
        }

        fn has(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Err(StoreError::Backend("broken".to_string()))
        }

        fn delete(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Err(StoreError::Backend("broken".to_string()))
        }
    }

    struct ScriptedStream {
        events: VecDeque<Result<StreamEvent, LlmError>>,
    }

    impl Stream for ScriptedStream {
        type Item = Result<StreamEvent, LlmError>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.events.pop_front())
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

    fn text_response(text: &str) -> Vec<Result<StreamEvent, LlmError>> {
        vec![
            Ok(StreamEvent::TextDelta {
                text: text.to_string(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::Stop,
            }),
        ]
    }

    fn config() -> AgentConfig {
        AgentConfig::new(
            ModelId::parse("test-model").expect("model"),
            PathBuf::from("/default"),
        )
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        file: ScheduleFile,
        cwd: PathBuf,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().expect("tempdir");
        let cwd = directory.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        let file = ScheduleFile::new(directory.path().join("schedules.json"));
        Fixture {
            _directory: directory,
            file,
            cwd,
        }
    }

    async fn run(
        fixture: &Fixture,
        store: Arc<dyn SessionStore>,
        provider: Arc<dyn LlmProvider>,
        clock: i64,
    ) -> Result<Vec<ScheduleRun>, ScheduleError> {
        run_due(
            &fixture.file,
            store,
            provider,
            ToolRegistry::new(),
            config(),
            Arc::new(FixedClock(clock)),
        )
        .await
    }

    #[tokio::test]
    async fn a_due_schedule_runs_in_a_fresh_session_and_advances() {
        let fixture = fixture();
        let cwd = fixture.cwd.display().to_string();
        let due = fixture
            .file
            .create("say hello", 60, None, cwd.clone(), 1_000_000)
            .expect("due");
        let future = fixture
            .file
            .create("later", 3_600, None, cwd.clone(), 1_000_000)
            .expect("future");
        let store = Arc::new(MemoryStore::default());
        let provider = Arc::new(ScriptedProvider::new(vec![text_response(
            "scheduled reply",
        )]));

        let runs = run(&fixture, store.clone(), provider, 1_060_000)
            .await
            .expect("run_due");

        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.schedule_id, due.id);
        assert!(run.session_id.as_str().starts_with("session-"));
        assert_eq!(run.outcome.text, "scheduled reply");
        assert!(run.outcome.is_success());
        assert!(store.has(&run.session_id).expect("has"));
        assert_eq!(
            store
                .header(&run.session_id)
                .expect("header")
                .cwd
                .as_deref(),
            Some(cwd.as_str())
        );
        assert!(future.next_run_at > 1_060_000);

        let stored = fixture.file.load().expect("load");
        let advanced = stored
            .iter()
            .find(|schedule| schedule.id == due.id)
            .expect("advanced");
        assert_eq!(advanced.next_run_at, 1_120_000);
        let untouched = stored
            .iter()
            .find(|schedule| schedule.id == future.id)
            .expect("untouched");
        assert_eq!(untouched.next_run_at, future.next_run_at);
        assert!(!runs.iter().any(|run| run.schedule_id == future.id));
    }

    #[tokio::test]
    async fn a_recorded_session_is_reused() {
        let fixture = fixture();
        let session_id = SessionId::parse("session-recorded").expect("id");
        fixture
            .file
            .create(
                "say hello",
                60,
                Some(session_id.clone()),
                fixture.cwd.display().to_string(),
                1_000,
            )
            .expect("create");
        let store = Arc::new(MemoryStore::default());
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("again")]));

        let runs = run(&fixture, store.clone(), provider, 61_000)
            .await
            .expect("run_due");

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].session_id, session_id);
        assert!(store.has(&session_id).expect("has"));
    }

    #[tokio::test]
    async fn a_failed_turn_is_a_run_result_and_still_advances() {
        let fixture = fixture();
        let schedule = fixture
            .file
            .create(
                "say hello",
                60,
                None,
                fixture.cwd.display().to_string(),
                1_000,
            )
            .expect("create");
        let store = Arc::new(MemoryStore::default());
        let provider = Arc::new(ScriptedProvider::new(vec![]));

        let runs = run(&fixture, store, provider, 61_000)
            .await
            .expect("run_due");

        assert_eq!(runs.len(), 1);
        assert!(!runs[0].outcome.is_success());
        let stored = fixture.file.load().expect("load");
        assert_eq!(stored[0].next_run_at, 121_000);
        assert_eq!(stored[0].id, schedule.id);
    }

    #[tokio::test]
    async fn storage_failures_stop_the_batch_before_advancing() {
        let fixture = fixture();
        let schedule = fixture
            .file
            .create(
                "say hello",
                60,
                None,
                fixture.cwd.display().to_string(),
                1_000,
            )
            .expect("create");
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));

        let error = run(&fixture, Arc::new(BrokenStore), provider, 61_000)
            .await
            .expect_err("broken store");

        assert!(matches!(
            error,
            ScheduleError::Agent(dsh_agent::AgentError::Store(_))
        ));
        assert_eq!(
            fixture.file.load().expect("load")[0].next_run_at,
            schedule.next_run_at
        );
    }

    #[tokio::test]
    async fn child_setup_failures_are_scheduler_errors() {
        let fixture = fixture();
        fixture
            .file
            .create(
                "say hello",
                60,
                None,
                fixture.cwd.display().to_string(),
                1_000,
            )
            .expect("create");
        let provider = Arc::new(ScriptedProvider {
            name: "",
            responses: Mutex::new(VecDeque::new()),
        });

        let error = run(&fixture, Arc::new(MemoryStore::default()), provider, 61_000)
            .await
            .expect_err("invalid provider");

        assert!(matches!(
            error,
            ScheduleError::Agent(dsh_agent::AgentError::InvalidProviderName(_))
        ));
    }

    #[tokio::test]
    async fn nothing_due_runs_nothing() {
        let fixture = fixture();
        fixture
            .file
            .create(
                "say hello",
                3_600,
                None,
                fixture.cwd.display().to_string(),
                1_000,
            )
            .expect("create");
        let store = Arc::new(MemoryStore::default());
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));

        let runs = run(&fixture, store.clone(), provider, 1_000)
            .await
            .expect("run_due");

        assert_eq!(runs, Vec::new());
        assert_eq!(store.list().expect("list"), Vec::new());
    }
}
