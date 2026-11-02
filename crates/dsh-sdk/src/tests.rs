use super::*;

use dsh_core::{
    EventRecord, EventSeq, FinishReason, FixedClock, LlmError, LlmRequest, LlmStream, ModelId,
    SessionEvent, SessionHeader, SessionSummary, StreamEvent, Tool, ToolContext, ToolOutcome,
    ToolSchema,
};
use proptest::prelude::*;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;

const CONFIG: &str = r#"{"jsonrpc":"2.0","id":1,"method":"config","params":{}}"#;
const LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"sessions.list","params":{}}"#;
const TOOLS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools.list","params":{}}"#;

fn config() -> AgentConfig {
    let mut config = AgentConfig::new(
        ModelId::parse("test-model").expect("model"),
        PathBuf::from("/work"),
    );
    config.system_prompt = "be useful".to_string();
    config.max_tokens = 128;
    config.max_steps = 4;
    config
}

fn state(store: Arc<MockStore>, provider: Arc<dyn LlmProvider>) -> SdkState {
    let mut tools = ToolRegistry::new();
    tools.insert(Arc::new(EchoTool)).expect("tool");
    SdkState {
        store,
        provider,
        tools,
        config: config(),
        clock: Arc::new(FixedClock(1_000)),
    }
}

fn empty_store() -> Arc<MockStore> {
    Arc::new(MockStore::default())
}

/// A buffer that only becomes readable when the writer flushes.
#[derive(Clone, Default)]
struct Written(Arc<StdMutex<Vec<u8>>>);

impl Written {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("written").clone()
    }
}

struct LineWriter {
    written: Written,
    pending: Vec<u8>,
}

impl LineWriter {
    fn new(written: Written) -> Self {
        Self {
            written,
            pending: Vec::new(),
        }
    }
}

impl AsyncWrite for LineWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pending.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.written
            .0
            .lock()
            .expect("written")
            .extend_from_slice(&self.pending);
        self.pending.clear();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

async fn drive(
    state: SdkState,
    requests: &str,
    cancel: CancellationToken,
) -> (Result<(), SdkError>, Written) {
    let written = Written::default();
    let result = serve_io(
        state,
        requests.as_bytes(),
        LineWriter::new(written.clone()),
        cancel,
    )
    .await;
    (result, written)
}

async fn call(state: SdkState, requests: &str) -> Vec<Value> {
    let (result, written) = drive(state, requests, CancellationToken::new()).await;
    result.expect("serve");
    frames(&written.bytes())
}

fn frames(output: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(output).expect("utf8");
    assert!(text.ends_with('\n'), "a frame ends its line");
    text.split_terminator('\n')
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect()
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    Append,
    List,
}

#[derive(Default)]
struct StoreState {
    sessions: BTreeMap<SessionId, (SessionHeader, Vec<EventRecord>)>,
    fault: Fault,
}

#[derive(Default)]
struct MockStore {
    state: StdMutex<StoreState>,
}

impl MockStore {
    fn set_fault(&self, fault: Fault) {
        self.state.lock().expect("lock").fault = fault;
    }
}

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
        drop(state);
        Ok(())
    }

    fn append(
        &self,
        session_id: &SessionId,
        at: i64,
        event: SessionEvent,
    ) -> Result<EventRecord, StoreError> {
        let mut state = self.state.lock().expect("lock");
        if state.fault == Fault::Append {
            return Err(StoreError::Backend("append failed".to_string()));
        }
        let Some((_, events)) = state.sessions.get_mut(session_id) else {
            return Err(StoreError::NotFound(session_id.clone()));
        };
        let record = EventRecord {
            seq: EventSeq::new(u64::try_from(events.len()).expect("length")),
            at,
            event,
        };
        events.push(record.clone());
        drop(state);
        Ok(record)
    }

    fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
        self.state
            .lock()
            .expect("lock")
            .sessions
            .get(session_id)
            .map(|(_, events)| events.clone())
            .ok_or_else(|| StoreError::NotFound(session_id.clone()))
    }

    fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let state = self.state.lock().expect("lock");
        if state.fault == Fault::List {
            return Err(StoreError::Backend("list failed".to_string()));
        }
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

struct ScriptedProvider {
    name: &'static str,
    responses: StdMutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Self {
        Self::named("scripted", responses)
    }

    fn named(name: &'static str, responses: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Self {
        Self {
            name,
            responses: StdMutex::new(responses.into()),
        }
    }
}

#[async_trait::async_trait]
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

fn text_provider(text: &str) -> Arc<dyn LlmProvider> {
    Arc::new(ScriptedProvider::new(vec![text_response(text)]))
}

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema::new("echo", "echo the arguments", json!({"type": "object"})).expect("schema")
    }

    async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
        ToolOutcome::text(arguments.to_string())
    }
}

fn request(method: &str, id: u64, params: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#)
}

#[tokio::test]
async fn config_reports_the_agent_settings() {
    let frames = call(state(empty_store(), text_provider("")), CONFIG).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["jsonrpc"], "2.0");
    assert_eq!(frames[0]["id"], 1);
    let result = &frames[0]["result"];
    assert_eq!(result["model"], "test-model");
    assert_eq!(result["max_steps"], 4);
    assert_eq!(result["max_tokens"], 128);
    assert_eq!(result["permission"], "workspace-write");
}

#[tokio::test]
async fn tools_list_answers_with_the_registered_tools() {
    let frames = call(state(empty_store(), text_provider("")), TOOLS).await;
    assert_eq!(frames[0]["id"], 2);
    let result = frames[0]["result"].as_array().expect("array");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["name"], "echo");
    assert_eq!(result[0]["description"], "echo the arguments");
    assert_eq!(result[0]["parameters"], json!({"type": "object"}));
}

#[tokio::test]
async fn sessions_list_answers_with_the_stored_sessions() {
    let store = empty_store();
    let header = SessionHeader::new(SessionId::generate(), 1_000, Some("/work".to_string()));
    store.create(&header).expect("create");
    let frames = call(state(Arc::clone(&store), text_provider("")), LIST).await;
    let result = frames[0]["result"].as_array().expect("array");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["id"], header.id.as_str());
    assert_eq!(result[0]["created_at"], 1_000);
    assert_eq!(result[0]["cwd"], "/work");
    assert_eq!(result[0]["event_count"], 1);
}

#[tokio::test]
async fn run_streams_events_before_the_turn_result() {
    let store = empty_store();
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello")]));
    let frames = call(
        state(Arc::clone(&store), provider),
        &request("run", 7, r#"{"task":"hi"}"#),
    )
    .await;
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0]["method"], "event");
    assert_eq!(frames[0]["params"]["type"], "session");
    assert_eq!(frames[1]["params"]["type"], "text");
    assert_eq!(frames[1]["params"]["text"], "hello");
    assert_eq!(frames[2]["params"]["type"], "final");
    assert_eq!(frames[2]["params"]["outcome"]["outcome"], "completed");
    assert_eq!(frames[3]["id"], 7);
    assert!(frames[3].get("method").is_none());
    let result = &frames[3]["result"];
    assert_eq!(result["text"], "hello");
    assert_eq!(result["outcome"]["outcome"], "completed");
    let session_id =
        SessionId::parse(result["session_id"].as_str().expect("session id")).expect("valid id");
    let records = store.load(&session_id).expect("load");
    assert_eq!(records.last().expect("last").event.kind(), "turn_ended");
}

#[tokio::test]
async fn run_reuses_a_requested_session() {
    let store = empty_store();
    let header = SessionHeader::new(SessionId::generate(), 1_000, None);
    store.create(&header).expect("create");
    let frames = call(
        state(Arc::clone(&store), text_provider("second")),
        &request(
            "run",
            3,
            &format!(r#"{{"task":"hi","session_id":"{}"}}"#, header.id),
        ),
    )
    .await;
    assert_eq!(frames[3]["result"]["session_id"], header.id.as_str());
    assert_eq!(frames[3]["result"]["text"], "second");
    let records = store.load(&header.id).expect("load");
    let turns = records
        .iter()
        .filter(|record| record.event.kind() == "turn_started")
        .count();
    assert_eq!(turns, 1);
}

#[tokio::test]
async fn sessions_show_returns_the_log() {
    let store = empty_store();
    let header = SessionHeader::new(SessionId::generate(), 1_000, None);
    store.create(&header).expect("create");
    let frames = call(
        state(Arc::clone(&store), text_provider("")),
        &request(
            "sessions.show",
            4,
            &format!(r#"{{"session_id":"{}"}}"#, header.id),
        ),
    )
    .await;
    let result = frames[0]["result"].as_array().expect("array");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["seq"], 0);
    assert_eq!(result[0]["event"]["type"], "session_created");
    assert_eq!(result[0]["event"]["created_at"], 1_000);
}

#[tokio::test]
async fn unknown_methods_are_method_not_found() {
    let frames = call(
        state(empty_store(), text_provider("")),
        &request("no.such.method", 5, "{}"),
    )
    .await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["id"], 5);
    assert_eq!(frames[0]["error"]["code"], -32601);
    assert_eq!(
        frames[0]["error"]["message"],
        r#"no method named "no.such.method""#
    );
}

#[tokio::test]
async fn malformed_json_is_a_parse_error() {
    let frames = call(state(empty_store(), text_provider("")), "{not json}\n").await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["id"], Value::Null);
    assert_eq!(frames[0]["error"]["code"], -32700);
}

#[tokio::test]
async fn a_missing_method_is_an_invalid_request() {
    let frames = call(
        state(empty_store(), text_provider("")),
        r#"{"jsonrpc":"2.0","id":11}
"#,
    )
    .await;
    assert_eq!(frames[0]["id"], 11);
    assert_eq!(frames[0]["error"]["code"], -32602);
    assert_eq!(
        frames[0]["error"]["message"],
        "request must carry a string method"
    );
}

#[tokio::test]
async fn missing_run_params_are_invalid() {
    let frames = call(
        state(empty_store(), text_provider("")),
        &request("run", 6, "{}"),
    )
    .await;
    assert_eq!(frames[0]["id"], 6);
    assert_eq!(frames[0]["error"]["code"], -32602);
}

#[tokio::test]
async fn an_unparseable_session_id_is_invalid() {
    let frames = call(
        state(empty_store(), text_provider("")),
        &request("sessions.show", 8, r#"{"session_id":""}"#),
    )
    .await;
    assert_eq!(frames[0]["id"], 8);
    assert_eq!(frames[0]["error"]["code"], -32602);
    assert!(
        frames[0]["error"]["message"]
            .as_str()
            .expect("message")
            .starts_with("invalid session id:")
    );
}

#[tokio::test]
async fn showing_a_missing_session_is_invalid() {
    let frames = call(
        state(empty_store(), text_provider("")),
        &request(
            "sessions.show",
            9,
            &format!(r#"{{"session_id":"{}"}}"#, SessionId::generate()),
        ),
    )
    .await;
    assert_eq!(frames[0]["id"], 9);
    assert_eq!(frames[0]["error"]["code"], -32602);
    assert!(
        frames[0]["error"]["message"]
            .as_str()
            .expect("message")
            .starts_with("no session session-")
    );
}

#[tokio::test]
async fn a_store_failure_is_an_internal_error() {
    let store = empty_store();
    store.set_fault(Fault::List);
    let frames = call(state(store, text_provider("")), LIST).await;
    assert_eq!(frames[0]["id"], 1);
    assert_eq!(frames[0]["error"]["code"], -32603);
    assert_eq!(
        frames[0]["error"]["message"],
        "storage failure: list failed"
    );
}

#[tokio::test]
async fn a_run_error_is_an_internal_error() {
    let store = empty_store();
    store.set_fault(Fault::Append);
    let frames = call(
        state(Arc::clone(&store), text_provider("never")),
        &request("run", 12, r#"{"task":"hi"}"#),
    )
    .await;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["params"]["type"], "session");
    assert_eq!(frames[1]["id"], 12);
    assert_eq!(frames[1]["error"]["code"], -32603);
    assert!(store.load(&SessionId::generate()).is_err());
}

#[tokio::test]
async fn a_provider_name_that_is_not_an_identifier_is_an_internal_error() {
    let provider = ScriptedProvider::named("", Vec::new());
    let frames = call(
        state(empty_store(), Arc::new(provider)),
        &request("run", 13, r#"{"task":"hi"}"#),
    )
    .await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["id"], 13);
    assert_eq!(frames[0]["error"]["code"], -32603);
    assert!(
        frames[0]["error"]["message"]
            .as_str()
            .expect("message")
            .starts_with("provider name")
    );
}

#[tokio::test]
async fn requests_are_answered_in_order() {
    let requests = format!("{CONFIG}\n{TOOLS}\n");
    let frames = call(state(empty_store(), text_provider("")), &requests).await;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["id"], 1);
    assert_eq!(frames[1]["id"], 2);
}

#[tokio::test]
async fn cancellation_stops_the_read_loop_before_any_request_runs() {
    let store = empty_store();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (result, written) = drive(
        state(Arc::clone(&store), text_provider("never")),
        &request("run", 14, r#"{"task":"hi"}"#),
        cancel,
    )
    .await;
    result.expect("serve");
    assert_eq!(written.bytes(), Vec::<u8>::new());
    assert_eq!(store.list().expect("list"), Vec::<SessionSummary>::new());
}

fn id_strategy() -> impl Strategy<Value = RequestId> {
    prop_oneof![
        any::<i64>().prop_map(|value| RequestId::Number(serde_json::Number::from(value))),
        "[A-Za-z0-9_-]{1,12}".prop_map(RequestId::String),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn error_responses_echo_the_request_id(id in id_strategy(), method in "[a-z][a-z.]{0,12}") {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {},
        });
        let (result, written) = runtime.block_on(drive(
            state(empty_store(), text_provider("")),
            &request.to_string(),
            CancellationToken::new(),
        ));
        result.expect("serve");
        let frames = frames(&written.bytes());
        prop_assert_eq!(frames.len(), 1);
        prop_assert_eq!(
            frames[0].get("id"),
            Some(&serde_json::to_value(&id).expect("id"))
        );
        prop_assert_eq!(
            frames[0]["error"]["code"].as_i64(),
            Some(-32601)
        );
    }
}
