//! The turn and step driver behind one headless run.

use crate::assembler::AssistantAssembler;
use dsh_core::config::{DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_MAX_STEPS, DEFAULT_MAX_TOKENS};
use dsh_core::{
    Clock, EventRecord, LlmError, LlmProvider, LlmRequest, Message, MessageId, ModelId,
    PermissionMode, ProviderId, ReasoningEffort, SessionEvent, SessionHeader, SessionId,
    SessionStore, StepId, StoreError, StreamEvent, TokenUsage, ToolCall, ToolContext, ToolMessage,
    ToolRegistry, TurnId, TurnOutcome, UserMessage, derive_messages,
};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Per-run model, policy, and workspace settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Route model id.
    pub model: ModelId,
    /// System prompt sent with every request.
    pub system_prompt: String,
    /// Per-request output cap.
    pub max_tokens: u32,
    /// Reasoning effort.
    pub effort: ReasoningEffort,
    /// Step budget for one turn.
    pub max_steps: u32,
    /// Maximum concurrent concurrency-safe tools.
    pub max_parallel_tools: usize,
    /// Effective permission mode.
    pub permission: PermissionMode,
    /// Physical session working directory.
    pub cwd: PathBuf,
}

impl AgentConfig {
    /// Build a config with the harness defaults.
    #[must_use]
    pub const fn new(model: ModelId, cwd: PathBuf) -> Self {
        Self {
            model,
            system_prompt: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            effort: ReasoningEffort::High,
            max_steps: DEFAULT_MAX_STEPS,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            permission: PermissionMode::WorkspaceWrite,
            cwd,
        }
    }
}

/// Failures that prevent the driver from recording a turn.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Durable storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The provider name is not a valid identifier.
    #[error("provider name {0:?} is not a valid identifier")]
    InvalidProviderName(String),
    /// The provider or transport failed.
    #[error("provider stream failed: {0}")]
    Stream(#[from] LlmError),
    /// The provider violated the streaming protocol.
    #[error("assistant stream is malformed: {0}")]
    MalformedStream(String),
    /// The provider ended without a terminal event.
    #[error("provider returned no terminal finish")]
    MissingFinish,
    /// The provider settled with no content.
    #[error("provider returned no content")]
    EmptyResponse,
}

/// A live frame for interactive and JSON consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// The session identity the run used.
    Session {
        /// Session identity.
        session_id: SessionId,
    },
    /// A short progress note.
    Status {
        /// Note text.
        text: String,
    },
    /// Visible answer text.
    Text {
        /// Newly arrived text.
        text: String,
    },
    /// Reasoning text.
    Thinking {
        /// Newly arrived reasoning.
        text: String,
    },
    /// A tool call opened.
    ToolCall {
        /// Call identity.
        id: dsh_core::ToolCallId,
        /// Tool name.
        name: String,
    },
    /// A tool call settled.
    ToolResult {
        /// Call identity.
        id: dsh_core::ToolCallId,
        /// Tool name.
        name: String,
        /// Model-facing output.
        output: String,
        /// Whether the call failed.
        is_error: bool,
    },
    /// The turn closed.
    Final {
        /// Session identity.
        session_id: SessionId,
        /// Terminal outcome.
        outcome: TurnOutcome,
        /// Last assistant text.
        text: String,
    },
    /// A failure occurred outside a turn.
    Error {
        /// Stable machine code.
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

/// Receiver for live run frames.
pub type EventSink = Arc<dyn Fn(RunEvent) + Send + Sync>;

/// The settled result of one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunOutcome {
    /// Session identity.
    pub session_id: SessionId,
    /// How the turn ended.
    pub outcome: TurnOutcome,
    /// Last assistant text.
    pub text: String,
    /// Accumulated usage.
    pub usage: TokenUsage,
    /// Steps executed.
    pub steps: u32,
}

impl RunOutcome {
    /// Whether the turn reached its normal terminal state.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.outcome.is_success()
    }
}

/// The default "call model, run tools, repeat" driver.
pub struct AgentLoop {
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn LlmProvider>,
    provider_id: ProviderId,
    tools: ToolRegistry,
    config: AgentConfig,
    clock: Arc<dyn Clock>,
}

impl AgentLoop {
    /// Build a driver from its seams.
    pub fn new(
        store: Arc<dyn SessionStore>,
        provider: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        config: AgentConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, AgentError> {
        let provider_id = ProviderId::parse(provider.name())
            .map_err(|_| AgentError::InvalidProviderName(provider.name().to_string()))?;
        Ok(Self {
            store,
            provider,
            provider_id,
            tools,
            config,
            clock,
        })
    }

    /// Run one turn against an existing or fresh session.
    #[allow(clippy::too_many_lines)]
    pub async fn run(
        &self,
        session_id: &SessionId,
        task: &str,
        cancel: CancellationToken,
        sink: Option<EventSink>,
    ) -> Result<RunOutcome, AgentError> {
        self.ensure_session(session_id)?;
        emit(
            &sink,
            RunEvent::Session {
                session_id: session_id.clone(),
            },
        );

        let mut messages = derive_messages(&self.store.load(session_id)?);
        let turn_id = TurnId::generate();
        self.append(
            session_id,
            SessionEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
        )?;
        let user = UserMessage::text(task);
        self.append(
            session_id,
            SessionEvent::UserMessage {
                message: user.clone(),
            },
        )?;
        messages.push(Message::User(user));

        let mut usage = TokenUsage::default();
        let mut final_text = String::new();
        let mut steps = 0u32;

        if cancel.is_cancelled() {
            return self.finish_turn(
                session_id,
                &turn_id,
                TurnOutcome::Cancelled {
                    reason: "cancelled before the first step".to_string(),
                },
                final_text,
                usage,
                steps,
                &sink,
            );
        }

        for _ in 0..self.config.max_steps {
            steps = steps.saturating_add(1);
            let step_id = StepId::generate();
            self.append(
                session_id,
                SessionEvent::StepStarted {
                    turn_id: turn_id.clone(),
                    step_id,
                },
            )?;

            let request = LlmRequest {
                model: self.config.model.clone(),
                system: Some(self.config.system_prompt.clone()),
                messages: messages.clone(),
                tools: self.tools.schemas(),
                max_tokens: self.config.max_tokens,
                effort: self.config.effort,
                temperature: None,
                stop: Vec::new(),
            };
            let mut stream = match self.provider.stream(request, cancel.clone()).await {
                Ok(stream) => stream,
                Err(error) => {
                    return self.fail_turn(
                        session_id, &turn_id, &error, final_text, usage, steps, &sink,
                    );
                }
            };

            let mut assembler = AssistantAssembler::new();
            let mut failure: Option<AgentError> = None;
            let mut cancelled = false;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    next = stream.next() => match next {
                        Some(Ok(event)) => {
                            emit_stream_event(&sink, &event);
                            if let Err(error) = assembler.push(event) {
                                failure = Some(error);
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            failure = Some(AgentError::Stream(error));
                            break;
                        }
                        None => break,
                    }
                }
            }

            if cancelled {
                return self.finish_turn(
                    session_id,
                    &turn_id,
                    TurnOutcome::Cancelled {
                        reason: "cancelled during the model stream".to_string(),
                    },
                    final_text,
                    usage,
                    steps,
                    &sink,
                );
            }
            if let Some(error) = failure {
                return self.fail_agent_error(
                    session_id, &turn_id, error, final_text, usage, steps, &sink,
                );
            }

            let assistant =
                match assembler.finish(self.provider_id.clone(), self.config.model.clone()) {
                    Ok(assistant) => assistant,
                    Err(error) => {
                        return self.fail_agent_error(
                            session_id, &turn_id, error, final_text, usage, steps, &sink,
                        );
                    }
                };
            usage.add(assistant.usage);
            final_text = assistant.text();
            let calls: Vec<ToolCall> = assistant.tool_calls().into_iter().cloned().collect();
            self.append(
                session_id,
                SessionEvent::AssistantMessage {
                    message: assistant.clone(),
                },
            )?;
            messages.push(Message::Assistant(assistant));

            if calls.is_empty() {
                return self.finish_turn(
                    session_id,
                    &turn_id,
                    TurnOutcome::Completed,
                    final_text,
                    usage,
                    steps,
                    &sink,
                );
            }

            let context = ToolContext::new(
                self.config.cwd.clone(),
                self.config.permission,
                cancel.clone(),
            );
            let results = self.execute_calls(&calls, &context, &cancel, &sink).await;
            for result in results {
                self.append(
                    session_id,
                    SessionEvent::ToolResult {
                        message: result.clone(),
                    },
                )?;
                messages.push(Message::Tool(result));
            }
            if cancel.is_cancelled() {
                return self.finish_turn(
                    session_id,
                    &turn_id,
                    TurnOutcome::Cancelled {
                        reason: "cancelled during tool execution".to_string(),
                    },
                    final_text,
                    usage,
                    steps,
                    &sink,
                );
            }
        }

        self.finish_turn(
            session_id,
            &turn_id,
            TurnOutcome::StepLimit,
            final_text,
            usage,
            steps,
            &sink,
        )
    }

    fn ensure_session(&self, session_id: &SessionId) -> Result<(), AgentError> {
        if self.store.has(session_id)? {
            return Ok(());
        }
        let header = SessionHeader::new(
            session_id.clone(),
            self.clock.now_millis(),
            Some(self.config.cwd.display().to_string()),
        );
        match self.store.create(&header) {
            Ok(()) | Err(StoreError::AlreadyExists(_)) => Ok(()),
            Err(error) => Err(AgentError::Store(error)),
        }
    }

    fn append(
        &self,
        session_id: &SessionId,
        event: SessionEvent,
    ) -> Result<EventRecord, AgentError> {
        self.store
            .append(session_id, self.clock.now_millis(), event)
            .map_err(AgentError::Store)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        outcome: TurnOutcome,
        text: String,
        usage: TokenUsage,
        steps: u32,
        sink: &Option<EventSink>,
    ) -> Result<RunOutcome, AgentError> {
        self.append(
            session_id,
            SessionEvent::TurnEnded {
                turn_id: turn_id.clone(),
                outcome: outcome.clone(),
            },
        )?;
        emit(
            sink,
            RunEvent::Final {
                session_id: session_id.clone(),
                outcome: outcome.clone(),
                text: text.clone(),
            },
        );
        Ok(RunOutcome {
            session_id: session_id.clone(),
            outcome,
            text,
            usage,
            steps,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        error: &LlmError,
        text: String,
        usage: TokenUsage,
        steps: u32,
        sink: &Option<EventSink>,
    ) -> Result<RunOutcome, AgentError> {
        emit(
            sink,
            RunEvent::Error {
                code: error.code.as_str().to_string(),
                message: error.message.clone(),
            },
        );
        self.finish_turn(
            session_id,
            turn_id,
            TurnOutcome::Failed {
                code: error.code.as_str().to_string(),
                message: error.message.clone(),
            },
            text,
            usage,
            steps,
            sink,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_agent_error(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        error: AgentError,
        text: String,
        usage: TokenUsage,
        steps: u32,
        sink: &Option<EventSink>,
    ) -> Result<RunOutcome, AgentError> {
        let (code, message) = match &error {
            AgentError::Stream(error) => {
                return self.fail_turn(session_id, turn_id, error, text, usage, steps, sink);
            }
            AgentError::MissingFinish => (
                "MALFORMED_RESPONSE",
                "provider returned no terminal finish".to_string(),
            ),
            AgentError::EmptyResponse => {
                ("EMPTY_RESPONSE", "provider returned no content".to_string())
            }
            AgentError::MalformedStream(detail) => ("MALFORMED_RESPONSE", detail.clone()),
            AgentError::Store(_) | AgentError::InvalidProviderName(_) => return Err(error),
        };
        emit(
            sink,
            RunEvent::Error {
                code: code.to_string(),
                message: message.clone(),
            },
        );
        self.finish_turn(
            session_id,
            turn_id,
            TurnOutcome::Failed {
                code: code.to_string(),
                message,
            },
            text,
            usage,
            steps,
            sink,
        )
    }

    async fn execute_calls(
        &self,
        calls: &[ToolCall],
        context: &ToolContext,
        cancel: &CancellationToken,
        sink: &Option<EventSink>,
    ) -> Vec<ToolMessage> {
        let mut results: Vec<Option<ToolMessage>> = vec![None; calls.len()];
        let safe: Vec<bool> = calls
            .iter()
            .map(|call| {
                self.tools
                    .get(&call.name)
                    .is_some_and(|tool| tool.concurrency_safe())
            })
            .collect();

        for batch in plan_batches(&safe) {
            if cancel.is_cancelled() {
                break;
            }
            if batch.len() == 1 {
                let index = batch[0];
                results[index] = Some(self.execute_one(&calls[index], context, sink).await);
            } else {
                let limit = self.config.max_parallel_tools.max(1);
                let completed: Vec<(usize, ToolMessage)> = stream::iter(batch.iter().copied())
                    .map(|index| {
                        let call = calls[index].clone();
                        let context = context.clone();
                        let sink = sink.clone();
                        async move { (index, self.execute_one(&call, &context, &sink).await) }
                    })
                    .buffer_unordered(limit)
                    .collect()
                    .await;
                for (index, result) in completed {
                    results[index] = Some(result);
                }
            }
        }

        calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                results[index].take().unwrap_or_else(|| {
                    ToolMessage::error(call.id.clone(), "Error: tool call aborted before dispatch")
                })
            })
            .collect()
    }

    async fn execute_one(
        &self,
        call: &ToolCall,
        context: &ToolContext,
        sink: &Option<EventSink>,
    ) -> ToolMessage {
        let Some(tool) = self.tools.get(&call.name) else {
            let text = format!("unknown tool: {}", call.name);
            emit(
                sink,
                RunEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    output: text.clone(),
                    is_error: true,
                },
            );
            return ToolMessage::error(call.id.clone(), text);
        };
        let outcome = tool.run(context, &call.arguments).await;
        let output = outcome.text_content();
        emit(
            sink,
            RunEvent::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                output,
                is_error: outcome.is_error,
            },
        );
        ToolMessage {
            id: MessageId::generate(),
            call_id: call.id.clone(),
            content: outcome.content,
            is_error: outcome.is_error,
        }
    }
}

/// Group calls into execution batches. Consecutive concurrency-safe calls share
/// a batch; every exclusive call is its own batch.
#[must_use]
pub fn plan_batches(concurrency_safe: &[bool]) -> Vec<Vec<usize>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    for (index, is_safe) in concurrency_safe.iter().enumerate() {
        if *is_safe {
            current.push(index);
        } else {
            if !current.is_empty() {
                batches.push(std::mem::take(&mut current));
            }
            batches.push(vec![index]);
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn emit(sink: &Option<EventSink>, event: RunEvent) {
    if let Some(sink) = sink {
        sink(event);
    }
}

fn emit_stream_event(sink: &Option<EventSink>, event: &StreamEvent) {
    match event {
        StreamEvent::TextDelta { text } if !text.is_empty() => {
            emit(sink, RunEvent::Text { text: text.clone() });
        }
        StreamEvent::ReasoningDelta { text } if !text.is_empty() => {
            emit(sink, RunEvent::Thinking { text: text.clone() });
        }
        StreamEvent::ToolCallStart { id, name } => {
            emit(
                sink,
                RunEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                },
            );
        }
        StreamEvent::TextDelta { .. }
        | StreamEvent::ReasoningDelta { .. }
        | StreamEvent::ToolCallDelta { .. }
        | StreamEvent::Usage { .. }
        | StreamEvent::Finish { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_core::{
        ContentBlock, EventSeq, FinishReason, FixedClock, LlmStream, SessionSummary, Tool,
        ToolCallId, ToolOutcome, ToolSchema,
    };
    use proptest::prelude::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

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
                seq: EventSeq::new(0),
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

    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
        requests: Mutex<Vec<LlmRequest>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
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
            self.requests.lock().expect("lock").push(request);
            let response = self
                .responses
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_default();
            Ok(Box::pin(stream::iter(response)))
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "echo",
                "echo arguments",
                serde_json::json!({"type": "object"}),
            )
            .expect("schema")
        }

        fn concurrency_safe(&self) -> bool {
            true
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            arguments: serde_json::Value,
        ) -> ToolOutcome {
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
        config
    }

    fn tools() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.insert(Arc::new(EchoTool)).expect("tool");
        tools
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

    fn tool_response(id: &str, name: &str, arguments: &str) -> Vec<Result<StreamEvent, LlmError>> {
        let id = ToolCallId::parse(id).expect("id");
        vec![
            Ok(StreamEvent::ToolCallStart {
                id: id.clone(),
                name: name.to_string(),
            }),
            Ok(StreamEvent::ToolCallDelta {
                id,
                arguments: arguments.to_string(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::ToolCalls,
            }),
        ]
    }

    struct FailingProvider;

    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn name(&self) -> &'static str {
            "failing"
        }

        async fn stream(
            &self,
            _request: LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            Err(LlmError::missing_credential("no key"))
        }
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn run_events_report_text_reasoning_tools_and_final() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Ok(StreamEvent::TextDelta {
                    text: String::new(),
                }),
                Ok(StreamEvent::TextDelta {
                    text: "hello".to_string(),
                }),
                Ok(StreamEvent::ReasoningDelta {
                    text: String::new(),
                }),
                Ok(StreamEvent::ReasoningDelta {
                    text: "why".to_string(),
                }),
                Ok(StreamEvent::ToolCallStart {
                    id: ToolCallId::parse("call-1").expect("id"),
                    name: "echo".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: ToolCallId::parse("call-1").expect("id"),
                    arguments: "{}".to_string(),
                }),
                Ok(StreamEvent::Finish {
                    reason: FinishReason::ToolCalls,
                }),
            ],
            text_response("done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store,
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |event| {
            captured.lock().expect("events").push(event);
        });
        loop_
            .run(&session_id, "hi", CancellationToken::new(), Some(sink))
            .await
            .expect("run");
        let events = events.lock().expect("events");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RunEvent::Session { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Text { text } if text == "hello"))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::Text { text } if text.is_empty()))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RunEvent::Thinking { text } if text == "why"))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::Thinking { text } if text.is_empty()))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RunEvent::ToolCall { name, .. } if name == "echo"))
        );
        assert!(
            events.iter().any(
                |event| matches!(event, RunEvent::ToolResult { output, .. } if output == "{}")
            )
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RunEvent::Final { .. }))
        );
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn provider_failure_emits_an_error_event() {
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store,
            Arc::new(FailingProvider),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |event| {
            captured.lock().expect("events").push(event);
        });
        loop_
            .run(&session_id, "hi", CancellationToken::new(), Some(sink))
            .await
            .expect("run");
        let events = events.lock().expect("events");
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::Error { code, .. } if code == "MISSING_CREDENTIAL"
        )));
    }

    #[tokio::test]
    async fn text_turn_is_logged_and_completed() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello")]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        assert_eq!(outcome.text, "hello");
        assert_eq!(outcome.steps, 1);

        let events = store.load(&session_id).expect("load");
        let kinds: Vec<_> = events.iter().map(|record| record.event.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                "session_created",
                "turn_started",
                "user_message",
                "step_started",
                "assistant_message",
                "turn_ended",
            ]
        );
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "hi");
        assert_eq!(messages[1].text(), "hello");
    }

    #[tokio::test]
    async fn tool_call_runs_and_second_step_finishes() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response("call-1", "echo", r#"{"value":1}"#),
            text_response("done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider.clone(),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "use the tool", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.steps, 2);

        let events = store.load(&session_id).expect("load");
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role(), dsh_core::message::Role::User);
        assert_eq!(messages[1].role(), dsh_core::message::Role::Assistant);
        assert_eq!(messages[2].role(), dsh_core::message::Role::Tool);
        assert_eq!(messages[3].text(), "done");
        assert_eq!(messages[2].text(), r#"{"value":1}"#);

        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system.as_deref(), Some("be useful"));
        assert_eq!(requests[0].tools[0].name, "echo");
    }

    #[tokio::test]
    async fn unknown_tools_are_reported_to_the_model() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response("call-1", "missing", "{}"),
            text_response("recovered"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider.clone(),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "call missing", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        let events = store.load(&session_id).expect("load");
        let tool_result = events
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.clone()),
                _ => None,
            })
            .expect("tool result");
        assert!(tool_result.is_error);
        assert_eq!(tool_result.text_content(), "unknown tool: missing");
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests[1].messages.len(), 3);
    }

    #[tokio::test]
    async fn provider_failure_closes_the_turn() {
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            Arc::new(FailingProvider),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(!outcome.is_success());
        assert!(matches!(
            outcome.outcome,
            TurnOutcome::Failed { ref code, .. } if code == "MISSING_CREDENTIAL"
        ));
        let events = store.load(&session_id).expect("load");
        assert_eq!(events.last().expect("last").event.kind(), "turn_ended");
    }

    #[tokio::test]
    async fn empty_response_closes_the_turn() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![Ok(StreamEvent::Finish {
            reason: FinishReason::Stop,
        })]]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(matches!(
            outcome.outcome,
            TurnOutcome::Failed { ref code, .. } if code == "EMPTY_RESPONSE"
        ));
    }

    #[tokio::test]
    async fn stream_failure_closes_the_turn() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![Err(LlmError::malformed(
            "broken",
        ))]]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(matches!(
            outcome.outcome,
            TurnOutcome::Failed { ref code, .. } if code == "MALFORMED_RESPONSE"
        ));
    }

    #[tokio::test]
    async fn step_limit_is_recorded() {
        let provider = Arc::new(ScriptedProvider::new(vec![tool_response(
            "call-1", "echo", "{}",
        )]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut config = config();
        config.max_steps = 1;
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config,
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "loop", CancellationToken::new(), None)
            .await
            .expect("run");
        assert_eq!(outcome.outcome, TurnOutcome::StepLimit);
        assert_eq!(outcome.steps, 1);
    }

    #[tokio::test]
    async fn cancellation_before_the_first_step_is_recorded() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = loop_
            .run(&session_id, "hi", cancel, None)
            .await
            .expect("run");
        assert!(matches!(outcome.outcome, TurnOutcome::Cancelled { .. }));
        let events = store.load(&session_id).expect("load");
        assert_eq!(events.last().expect("last").event.kind(), "turn_ended");
    }

    #[tokio::test]
    async fn resuming_a_session_appends_another_turn() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            text_response("first"),
            text_response("second"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider.clone(),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        loop_
            .run(&session_id, "one", CancellationToken::new(), None)
            .await
            .expect("first");
        let outcome = loop_
            .run(&session_id, "two", CancellationToken::new(), None)
            .await
            .expect("second");
        assert_eq!(outcome.text, "second");
        let events = store.load(&session_id).expect("load");
        let turn_starts = events
            .iter()
            .filter(|record| record.event.kind() == "turn_started")
            .count();
        assert_eq!(turn_starts, 2);
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests[1].messages.len(), 3);
    }

    #[tokio::test]
    async fn parallel_safe_calls_keep_model_order() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Ok(StreamEvent::ToolCallStart {
                    id: ToolCallId::parse("call-1").expect("id"),
                    name: "echo".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: ToolCallId::parse("call-1").expect("id"),
                    arguments: r#"{"n":1}"#.to_string(),
                }),
                Ok(StreamEvent::ToolCallStart {
                    id: ToolCallId::parse("call-2").expect("id"),
                    name: "echo".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: ToolCallId::parse("call-2").expect("id"),
                    arguments: r#"{"n":2}"#.to_string(),
                }),
                Ok(StreamEvent::Finish {
                    reason: FinishReason::ToolCalls,
                }),
            ],
            text_response("done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "two calls", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        let events = store.load(&session_id).expect("load");
        let tool_results: Vec<_> = events
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 2);
        assert_eq!(tool_results[0].call_id.as_str(), "call-1");
        assert_eq!(tool_results[0].text_content(), r#"{"n":1}"#);
        assert_eq!(tool_results[1].call_id.as_str(), "call-2");
        assert_eq!(tool_results[1].text_content(), r#"{"n":2}"#);
    }

    #[test]
    fn batch_planning_groups_only_consecutive_safe_calls() {
        assert_eq!(plan_batches(&[]), Vec::<Vec<usize>>::new());
        assert_eq!(plan_batches(&[false]), vec![vec![0]]);
        assert_eq!(
            plan_batches(&[true, true, false, true]),
            vec![vec![0, 1], vec![2], vec![3]]
        );
        assert_eq!(
            plan_batches(&[true, false, true, true]),
            vec![vec![0], vec![1], vec![2, 3]]
        );
    }

    proptest! {
        #[test]
        fn batch_planning_covers_every_index_once(safe in prop::collection::vec(any::<bool>(), 0..32)) {
            let batches = plan_batches(&safe);
            let mut indices: Vec<usize> = batches.iter().flatten().copied().collect();
            indices.sort_unstable();
            prop_assert_eq!(indices, (0..safe.len()).collect::<Vec<_>>());
            prop_assert!(batches.iter().all(|batch| !batch.is_empty()));
        }
    }

    #[test]
    fn run_outcome_success_tracks_completion() {
        let outcome = RunOutcome {
            session_id: SessionId::generate(),
            outcome: TurnOutcome::Completed,
            text: "ok".to_string(),
            usage: TokenUsage::default(),
            steps: 1,
        };
        assert!(outcome.is_success());
        assert_eq!(ContentBlock::text("x").as_text(), Some("x"));
    }
}
