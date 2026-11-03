//! The turn and step driver behind one headless run.

use crate::assembler::AssistantAssembler;
use dsh_core::config::{DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_MAX_STEPS, DEFAULT_MAX_TOKENS};
use dsh_core::{
    Clock, EventRecord, LlmError, LlmProvider, LlmRequest, Message, MessageId, ModelId, NoSandbox,
    PermissionMode, ProviderId, ReasoningEffort, Sandbox, SessionEvent, SessionHeader, SessionId,
    SessionStore, StepId, StoreError, StreamEvent, TokenUsage, ToolCall, ToolContext, ToolMessage,
    ToolRegistry, ToolSession, TurnId, TurnOutcome, UserMessage, derive_messages,
};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Per-run model, policy, and workspace settings.
#[derive(Clone)]
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
    /// Harness state directory for durable non-session files.
    pub state_dir: PathBuf,
    /// Retryable provider failures retried before the turn fails.
    pub max_retries: u32,
    /// Base delay for exponential retry backoff.
    pub retry_base_ms: u64,
    /// Estimated history characters that trigger compaction.
    pub max_context_chars: usize,
    /// Execution confinement mode.
    pub sandbox: Arc<dyn Sandbox>,
    /// Direct creator of a child session.
    pub parent_session: Option<SessionId>,
}

impl AgentConfig {
    /// Build a config with the harness defaults.
    #[must_use]
    pub fn new(model: ModelId, cwd: PathBuf) -> Self {
        Self {
            model,
            system_prompt: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            effort: ReasoningEffort::High,
            max_steps: DEFAULT_MAX_STEPS,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            permission: PermissionMode::WorkspaceWrite,
            cwd: cwd.clone(),
            state_dir: cwd,
            max_retries: 2,
            retry_base_ms: 500,
            max_context_chars: 200_000,
            sandbox: Arc::new(NoSandbox),
            parent_session: None,
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
    /// A fragment of a tool call's JSON arguments.
    ToolCallArgs {
        id: dsh_core::ToolCallId,
        /// Raw argument text as the provider sent it.
        arguments: String,
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
    /// A retryable provider failure scheduled another attempt.
    Retry {
        /// One-based attempt number.
        attempt: u32,
        /// Machine code of the failure.
        code: String,
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
        let session_handle: Arc<dyn ToolSession> = Arc::new(SessionHandle {
            store: Arc::clone(&self.store),
            session_id: session_id.clone(),
            clock: Arc::clone(&self.clock),
        });
        let turn_id = TurnId::generate();
        self.append(
            session_id,
            SessionEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
        )?;
        let user = UserMessage::text(task);
        let turn_start = self.append(
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
            self.maybe_compact(session_id, &mut messages, turn_start.seq)?;
            let cwd = session_cwd(&self.store.load(session_id)?, &self.config.cwd);
            let step_id = StepId::generate();
            self.append(
                session_id,
                SessionEvent::StepStarted {
                    turn_id: turn_id.clone(),
                    step_id: step_id.clone(),
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
            let mut stream = None;
            for attempt in 0..=self.config.max_retries {
                match self.provider.stream(request.clone(), cancel.clone()).await {
                    Ok(value) => {
                        stream = Some(value);
                        break;
                    }
                    Err(_) if cancel.is_cancelled() => {
                        return self.finish_turn(
                            session_id,
                            &turn_id,
                            TurnOutcome::Cancelled {
                                reason: "cancelled while opening the model stream".to_string(),
                            },
                            final_text,
                            usage,
                            steps,
                            &sink,
                        );
                    }
                    Err(error) if error.is_retryable() && attempt < self.config.max_retries => {
                        let attempt_number = attempt.saturating_add(1);
                        self.append(
                            session_id,
                            SessionEvent::RetryAttempt {
                                step_id: step_id.clone(),
                                attempt: attempt_number,
                                code: error.code.as_str().to_string(),
                            },
                        )?;
                        emit(
                            &sink,
                            RunEvent::Retry {
                                attempt: attempt_number,
                                code: error.code.as_str().to_string(),
                            },
                        );
                        let delay = retry_delay(self.config.retry_base_ms, attempt_number);
                        tokio::select! {
                            () = cancel.cancelled() => {
                                return self.finish_turn(
                                    session_id,
                                    &turn_id,
                                    TurnOutcome::Cancelled {
                                        reason: "cancelled during retry backoff".to_string(),
                                    },
                                    final_text,
                                    usage,
                                    steps,
                                    &sink,
                                );
                            }
                            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
                        }
                    }
                    Err(error) => {
                        return self.fail_turn(
                            session_id, &turn_id, &error, final_text, usage, steps, &sink,
                        );
                    }
                }
            }
            let Some(mut stream) = stream else {
                let error = LlmError::server("retry budget exhausted");
                return self.fail_turn(
                    session_id, &turn_id, &error, final_text, usage, steps, &sink,
                );
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
                            // A provider reports the aborted request as a transport
                            // failure, so the token decides whether this was a stop.
                            if cancel.is_cancelled() {
                                cancelled = true;
                            } else {
                                failure = Some(AgentError::Stream(error));
                            }
                            break;
                        }
                        None => {
                            if cancel.is_cancelled() {
                                cancelled = true;
                            }
                            break;
                        }
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

            let context = ToolContext::new(cwd, self.config.permission, cancel.clone())
                .with_state_dir(self.config.state_dir.clone())
                .with_sandbox(Arc::clone(&self.config.sandbox))
                .with_session(Arc::clone(&session_handle));
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
        let header = SessionHeader {
            parent_session: self.config.parent_session.clone(),
            ..SessionHeader::new(
                session_id.clone(),
                self.clock.now_millis(),
                Some(self.config.cwd.display().to_string()),
            )
        };
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

    fn maybe_compact(
        &self,
        session_id: &SessionId,
        messages: &mut Vec<Message>,
        keep_from: dsh_core::EventSeq,
    ) -> Result<(), AgentError> {
        if message_chars(messages) <= self.config.max_context_chars {
            return Ok(());
        }
        let events = self.store.load(session_id)?;
        let older: Vec<Message> = events
            .iter()
            .filter(|record| record.seq < keep_from)
            .filter_map(|record| record.event.message())
            .collect();
        if older.is_empty() {
            return Ok(());
        }
        let summary = summarize_messages(&older);
        self.append(session_id, SessionEvent::Compaction { summary, keep_from })?;
        *messages = derive_messages(&self.store.load(session_id)?);
        Ok(())
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

struct SessionHandle {
    store: Arc<dyn SessionStore>,
    session_id: SessionId,
    clock: Arc<dyn Clock>,
}

impl ToolSession for SessionHandle {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn append(&self, event: SessionEvent) -> Result<EventRecord, StoreError> {
        self.store
            .append(&self.session_id, self.clock.now_millis(), event)
    }

    fn now_millis(&self) -> i64 {
        self.clock.now_millis()
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<dsh_core::SessionSearchHit>, StoreError> {
        self.store.search(query, limit)
    }
}

fn message_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            let text = message.text().chars().count();
            let arguments: usize = message
                .tool_calls()
                .iter()
                .map(|call| call.arguments.chars().count())
                .sum();
            text.saturating_add(arguments)
        })
        .sum()
}

fn summarize_messages(messages: &[Message]) -> String {
    let mut summary = String::new();
    for message in messages {
        let text: String = message.text().chars().take(300).collect();
        summary.push_str(message.role().as_str());
        summary.push_str(": ");
        summary.push_str(&text);
        summary.push('\n');
    }
    summary.chars().take(4000).collect()
}

/// The directory this session's tools run in.
///
/// An empty recorded value is skipped because `Path::starts_with("")` is true
/// for every path, so it would defeat workspace containment.
fn session_cwd(events: &[EventRecord], fallback: &Path) -> PathBuf {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            SessionEvent::WorkingDirectoryChanged { cwd } if !cwd.is_empty() => {
                Some(PathBuf::from(cwd))
            }
            SessionEvent::SessionCreated { cwd, .. } => cwd
                .as_deref()
                .filter(|cwd| !cwd.is_empty())
                .map(PathBuf::from),
            _ => None,
        })
        .unwrap_or_else(|| fallback.to_path_buf())
}

fn retry_delay(base_ms: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(20);
    base_ms.saturating_mul(1u64 << shift)
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
        StreamEvent::ToolCallDelta { id, arguments } if !arguments.is_empty() => {
            emit(
                sink,
                RunEvent::ToolCallArgs {
                    id: id.clone(),
                    arguments: arguments.clone(),
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

        fn header(&self, session_id: &SessionId) -> Result<SessionHeader, StoreError> {
            let state = self.state.lock().expect("lock");
            state
                .sessions
                .get(session_id)
                .map(|(header, _)| header.clone())
                .ok_or_else(|| StoreError::NotFound(session_id.clone()))
        }

        fn search(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<dsh_core::SessionSearchHit>, StoreError> {
            if query == "hit" && limit > 0 {
                Ok(vec![dsh_core::SessionSearchHit {
                    session_id: SessionId::generate(),
                    seq: dsh_core::EventSeq::new(1),
                    kind: "user_message".to_string(),
                    snippet: "hit".to_string(),
                }])
            } else {
                Ok(Vec::new())
            }
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

    struct RetryProvider {
        failures: Mutex<VecDeque<LlmError>>,
        responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
        requests: Mutex<Vec<LlmRequest>>,
    }

    impl RetryProvider {
        fn new(
            failures: Vec<LlmError>,
            responses: Vec<Vec<Result<StreamEvent, LlmError>>>,
        ) -> Self {
            Self {
                failures: Mutex::new(failures.into()),
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for RetryProvider {
        fn name(&self) -> &'static str {
            "retry"
        }

        async fn stream(
            &self,
            request: LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            self.requests.lock().expect("requests").push(request);
            if let Some(error) = self.failures.lock().expect("failures").pop_front() {
                return Err(error);
            }
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or_default();
            Ok(Box::pin(stream::iter(response)))
        }
    }

    #[tokio::test]
    async fn retryable_failures_are_retried_and_logged() {
        let provider = Arc::new(RetryProvider::new(
            vec![LlmError::server("temporary")],
            vec![text_response("recovered")],
        ));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut config = config();
        config.max_retries = 2;
        config.retry_base_ms = 1;
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config,
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        assert_eq!(outcome.text, "recovered");
        let events = store.load(&session_id).expect("load");
        assert!(
            events
                .iter()
                .any(|record| record.event.kind() == "retry_attempt")
        );
    }

    #[tokio::test]
    async fn retry_exhaustion_closes_the_turn() {
        let provider = Arc::new(RetryProvider::new(
            vec![
                LlmError::server("one"),
                LlmError::server("two"),
                LlmError::server("three"),
            ],
            Vec::new(),
        ));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut config = config();
        config.max_retries = 1;
        config.retry_base_ms = 1;
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config,
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(matches!(
            outcome.outcome,
            TurnOutcome::Failed { ref code, .. } if code == "SERVER_ERROR"
        ));
        let retries = store
            .load(&session_id)
            .expect("load")
            .iter()
            .filter(|record| record.event.kind() == "retry_attempt")
            .count();
        assert_eq!(retries, 1);
    }

    #[tokio::test]
    async fn terminal_failures_are_not_retried() {
        let provider = Arc::new(RetryProvider::new(
            vec![LlmError::missing_credential("no key")],
            Vec::new(),
        ));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut config = config();
        config.max_retries = 3;
        config.retry_base_ms = 1;
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config,
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(matches!(
            outcome.outcome,
            TurnOutcome::Failed { ref code, .. } if code == "MISSING_CREDENTIAL"
        ));
        let events = store.load(&session_id).expect("load");
        assert!(
            !events
                .iter()
                .any(|record| record.event.kind() == "retry_attempt")
        );
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn long_histories_are_compacted_before_the_next_request() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            text_response("first"),
            text_response("second"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut config = config();
        config.max_context_chars = 10;
        let loop_ = AgentLoop::new(
            store.clone(),
            provider.clone(),
            tools(),
            config,
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        loop_
            .run(&session_id, "first task", CancellationToken::new(), None)
            .await
            .expect("first");
        let outcome = loop_
            .run(&session_id, "second task", CancellationToken::new(), None)
            .await
            .expect("second");
        assert_eq!(outcome.text, "second");
        let events = store.load(&session_id).expect("load");
        assert!(
            events
                .iter()
                .any(|record| record.event.kind() == "compaction")
        );
        let requests = provider.requests.lock().expect("requests");
        let second_request = &requests[1];
        assert_eq!(second_request.messages.len(), 2);
        assert!(second_request.messages[0].text().contains("Summary"));
        assert!(second_request.messages[0].text().contains("first task"));
        assert!(!second_request.messages[0].text().contains("second task"));
        assert_eq!(second_request.messages[1].text(), "second task");
    }

    #[test]
    fn retry_delays_grow_with_a_ceiling() {
        assert_eq!(retry_delay(100, 1), 100);
        assert_eq!(retry_delay(100, 2), 200);
        assert_eq!(retry_delay(100, 3), 400);
        assert_eq!(retry_delay(100, 21), 100 * (1u64 << 20));
        assert_eq!(retry_delay(0, 5), 0);
    }

    #[test]
    fn session_handles_report_clock_and_search() {
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let handle = SessionHandle {
            store,
            session_id,
            clock: Arc::new(FixedClock(1_234)),
        };
        assert_eq!(handle.now_millis(), 1_234);
        assert_eq!(handle.search("hit", 1).expect("search").len(), 1);
        assert_eq!(handle.search("miss", 1).expect("search"), Vec::new());
    }

    #[tokio::test]
    async fn parent_sessions_are_recorded() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("child")]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let parent = SessionId::generate();
        let mut config = config();
        config.parent_session = Some(parent.clone());
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            tools(),
            config,
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        loop_
            .run(&session_id, "child task", CancellationToken::new(), None)
            .await
            .expect("run");
        assert_eq!(
            store.header(&session_id).expect("header").parent_session,
            Some(parent)
        );
    }

    #[tokio::test]
    async fn subagents_run_in_child_sessions() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response("call-1", "subagent", r#"{"task":"child task"}"#),
            text_response("child done"),
            text_response("parent done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut registry = tools();
        let subagent = Arc::new(crate::SubagentTool::new(
            store.clone(),
            provider.clone(),
            registry.clone(),
            config(),
            Arc::new(FixedClock(1_000)),
        ));
        registry.insert(subagent).expect("subagent");
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            registry,
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "delegate", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        assert_eq!(outcome.text, "parent done");
        let parent_events = store.load(&session_id).expect("parent events");
        let tool_result = parent_events
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.clone()),
                _ => None,
            })
            .expect("tool result");
        assert!(tool_result.text_content().contains("child done"));
        let child = store
            .list()
            .expect("list")
            .into_iter()
            .find(|summary| summary.id != session_id)
            .expect("child session");
        assert_eq!(
            store
                .header(&child.id)
                .expect("child header")
                .parent_session,
            Some(session_id)
        );
        let child_messages = derive_messages(&store.load(&child.id).expect("child events"));
        assert!(
            child_messages
                .iter()
                .any(|message| message.text() == "child task")
        );
        assert!(
            child_messages
                .iter()
                .any(|message| message.text() == "child done")
        );
    }

    #[tokio::test]
    async fn workflows_run_ordered_child_sessions() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response(
                "call-1",
                "workflow",
                r#"{"steps":[{"task":"one"},{"task":"two"}]}"#,
            ),
            text_response("one"),
            text_response("two"),
            text_response("parent done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut registry = tools();
        let workflow = Arc::new(crate::WorkflowTool::new(
            store.clone(),
            provider.clone(),
            registry.clone(),
            config(),
            Arc::new(FixedClock(1_000)),
        ));
        registry.insert(workflow).expect("workflow");
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            registry,
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "workflow", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        assert_eq!(outcome.text, "parent done");
        let parent_events = store.load(&session_id).expect("parent events");
        let tool_result = parent_events
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.clone()),
                _ => None,
            })
            .expect("tool result");
        assert!(
            tool_result
                .text_content()
                .contains("workflow completed 2/2 steps")
        );
        assert!(tool_result.text_content().contains("one"));
        assert!(tool_result.text_content().contains("two"));
        let summaries = store.list().expect("list");
        assert_eq!(summaries.len(), 3);
        let mut children = summaries
            .into_iter()
            .filter(|summary| summary.id != session_id)
            .collect::<Vec<_>>();
        children.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        assert_eq!(children.len(), 2);
        for child in children {
            assert_eq!(
                store
                    .header(&child.id)
                    .expect("child header")
                    .parent_session,
                Some(session_id.clone())
            );
        }
    }

    #[tokio::test]
    async fn workflow_default_stops_after_a_failed_step() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response(
                "call-1",
                "workflow",
                r#"{"steps":[{"task":"fail"},{"task":"succeed"}]}"#,
            ),
            vec![Ok(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })],
            text_response("two"),
            text_response("parent done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut registry = tools();
        let workflow = Arc::new(crate::WorkflowTool::new(
            store.clone(),
            provider.clone(),
            registry.clone(),
            config(),
            Arc::new(FixedClock(1_000)),
        ));
        registry.insert(workflow).expect("workflow");
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            registry,
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        loop_
            .run(&session_id, "workflow", CancellationToken::new(), None)
            .await
            .expect("run");
        let parent_events = store.load(&session_id).expect("parent events");
        let tool_result = parent_events
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.clone()),
                _ => None,
            })
            .expect("tool result");
        assert!(
            tool_result
                .text_content()
                .contains("workflow completed 0/1 steps")
        );
        assert_eq!(store.list().expect("list").len(), 2);
    }

    #[tokio::test]
    async fn workflow_validates_step_counts_before_requiring_a_session() {
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tool = crate::WorkflowTool::new(
            Arc::new(MockStore::default()),
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        );
        let context = ToolContext::new(
            PathBuf::from("/work"),
            PermissionMode::WorkspaceWrite,
            CancellationToken::new(),
        );
        let empty = tool
            .execute(&context, serde_json::json!({"steps": []}))
            .await;
        assert!(empty.is_error);
        assert!(empty.text_content().contains("between 1 and 20"));

        let too_many: Vec<_> = (0..21)
            .map(|index| serde_json::json!({"task": format!("step {index}")}))
            .collect();
        let outcome = tool
            .execute(&context, serde_json::json!({"steps": too_many}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("between 1 and 20"));

        let twenty: Vec<_> = (0..20)
            .map(|index| serde_json::json!({"task": format!("step {index}")}))
            .collect();
        let outcome = tool
            .execute(&context, serde_json::json!({"steps": twenty}))
            .await;
        assert!(outcome.is_error);
        assert!(!outcome.text_content().contains("between 1 and 20"));
    }

    #[tokio::test]
    async fn ralph_runs_fresh_child_rounds() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response("call-1", "ralph", r#"{"objective":"ship","rounds":2}"#),
            text_response("one"),
            text_response("two"),
            text_response("parent done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut registry = tools();
        let ralph = Arc::new(crate::RalphTool::new(
            store.clone(),
            provider.clone(),
            registry.clone(),
            config(),
            Arc::new(FixedClock(1_000)),
        ));
        registry.insert(ralph).expect("ralph");
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            registry,
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        loop_
            .run(&session_id, "ralph", CancellationToken::new(), None)
            .await
            .expect("run");
        let parent_events = store.load(&session_id).expect("parent events");
        let tool_result = parent_events
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.clone()),
                _ => None,
            })
            .expect("tool result");
        assert!(
            tool_result
                .text_content()
                .contains("ralph completed 2/2 rounds")
        );
        assert!(tool_result.text_content().contains("one"));
        assert!(tool_result.text_content().contains("two"));
        assert_eq!(store.list().expect("list").len(), 3);
    }

    #[tokio::test]
    async fn ralph_validates_its_objective_and_round_count() {
        let tool = crate::RalphTool::new(
            Arc::new(MockStore::default()),
            Arc::new(ScriptedProvider::new(Vec::new())),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        );
        let context = ToolContext::new(
            PathBuf::from("/work"),
            PermissionMode::WorkspaceWrite,
            CancellationToken::new(),
        );
        for arguments in [
            serde_json::json!({"objective": " "}),
            serde_json::json!({"objective": "ship", "rounds": 0}),
            serde_json::json!({"objective": "ship", "rounds": 11}),
        ] {
            let outcome = tool.execute(&context, arguments).await;
            assert!(outcome.is_error);
            assert!(
                outcome.text_content().contains("must not be empty")
                    || outcome.text_content().contains("between 1 and 10")
            );
        }
        let ten = tool
            .execute(
                &context,
                serde_json::json!({"objective": "ship", "rounds": 10}),
            )
            .await;
        assert!(ten.is_error);
        assert!(!ten.text_content().contains("between 1 and 10"));
    }

    #[tokio::test]
    async fn subagents_require_a_session() {
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let store = Arc::new(MockStore::default());
        let tool = crate::SubagentTool::new(
            store,
            provider,
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        );
        let context = ToolContext::new(
            PathBuf::from("/work"),
            PermissionMode::WorkspaceWrite,
            CancellationToken::new(),
        );
        let outcome = tool
            .execute(&context, serde_json::json!({"task": "child"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("durable session"));
    }

    struct ChangeDirTool;
    struct PwdTool;

    #[async_trait]
    impl Tool for ChangeDirTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "change_dir",
                "change the session directory",
                serde_json::json!({"type": "object"}),
            )
            .expect("schema")
        }

        async fn execute(
            &self,
            context: &ToolContext,
            arguments: serde_json::Value,
        ) -> ToolOutcome {
            let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str) else {
                return ToolOutcome::error("path is required");
            };
            let Some(session) = context.session() else {
                return ToolOutcome::error("session is required");
            };
            match session.append(SessionEvent::WorkingDirectoryChanged {
                cwd: path.to_string(),
            }) {
                Ok(_) => ToolOutcome::text(format!("changed to {path}")),
                Err(error) => ToolOutcome::error(error.to_string()),
            }
        }
    }

    #[async_trait]
    impl Tool for PwdTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("pwd", "read the session directory", serde_json::json!({}))
                .expect("schema")
        }

        async fn execute(
            &self,
            context: &ToolContext,
            _arguments: serde_json::Value,
        ) -> ToolOutcome {
            ToolOutcome::text(context.cwd.display().to_string())
        }
    }

    #[tokio::test]
    async fn working_directory_changes_apply_to_later_steps() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response("call-1", "change_dir", r#"{"path":"/tmp/next"}"#),
            tool_response("call-2", "pwd", "{}"),
            text_response("done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let mut registry = tools();
        registry.insert(Arc::new(ChangeDirTool)).expect("change");
        registry.insert(Arc::new(PwdTool)).expect("pwd");
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            registry,
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "move", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        let events = store.load(&session_id).expect("load");
        let results: Vec<_> = events
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], "changed to /tmp/next");
        assert_eq!(results[1], "/tmp/next");
    }

    fn record(seq: u64, event: SessionEvent) -> EventRecord {
        EventRecord {
            seq: EventSeq::new(seq),
            at: 0,
            event,
        }
    }

    fn created(cwd: Option<&str>) -> SessionEvent {
        SessionEvent::SessionCreated {
            cwd: cwd.map(str::to_string),
            created_at: 0,
            format_version: 1,
        }
    }

    fn changed(cwd: &str) -> SessionEvent {
        SessionEvent::WorkingDirectoryChanged {
            cwd: cwd.to_string(),
        }
    }

    #[test]
    fn session_cwd_prefers_the_newest_directory_change() {
        let events = [
            record(0, created(Some("/created"))),
            record(1, changed("/older")),
            record(2, changed("/newer")),
        ];
        assert_eq!(
            session_cwd(&events, Path::new("/fallback")),
            PathBuf::from("/newer")
        );
    }

    #[test]
    fn session_cwd_falls_back_to_the_creation_directory() {
        let events = [record(0, created(Some("/created")))];
        assert_eq!(
            session_cwd(&events, Path::new("/fallback")),
            PathBuf::from("/created")
        );
    }

    #[test]
    fn session_cwd_falls_back_to_the_configured_directory() {
        let events = [record(0, created(None))];
        assert_eq!(
            session_cwd(&events, Path::new("/fallback")),
            PathBuf::from("/fallback")
        );
        assert_eq!(
            session_cwd(&[], Path::new("/fallback")),
            PathBuf::from("/fallback")
        );
    }

    #[test]
    fn session_cwd_skips_an_empty_directory_change() {
        let events = [record(0, created(Some("/created"))), record(1, changed(""))];
        assert_eq!(
            session_cwd(&events, Path::new("/fallback")),
            PathBuf::from("/created")
        );
    }

    #[tokio::test]
    async fn a_session_runs_tools_in_its_recorded_directory() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response("call-1", "pwd", "{}"),
            text_response("done"),
        ]));
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        store
            .create(&SessionHeader::new(
                session_id.clone(),
                1,
                Some("/project".to_string()),
            ))
            .expect("create");
        let mut registry = tools();
        registry.insert(Arc::new(PwdTool)).expect("pwd");
        let loop_ = AgentLoop::new(
            store.clone(),
            provider,
            registry,
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "where am I", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(outcome.is_success());
        let results: Vec<_> = store
            .load(&session_id)
            .expect("load")
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ToolResult { message } => Some(message.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ["/project"]);
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
    async fn tool_call_argument_fragments_reach_the_sink() {
        let id = ToolCallId::parse("call-1").expect("id");
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Ok(StreamEvent::ToolCallStart {
                    id: id.clone(),
                    name: "echo".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    arguments: String::new(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    arguments: r#"{"value""#.to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    arguments: ":1}".to_string(),
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
        let fragments: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ToolCallArgs { arguments, .. } => Some(arguments.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fragments, [r#"{"value""#, ":1}"]);
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::ToolCallArgs { id: call, .. } if call == &id
        )));
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

    struct CancelOnOpen;

    #[async_trait]
    impl LlmProvider for CancelOnOpen {
        fn name(&self) -> &'static str {
            "cancel-on-open"
        }

        async fn stream(
            &self,
            _request: LlmRequest,
            cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            cancel.cancel();
            Err(LlmError::transport("Messages request cancelled"))
        }
    }

    struct CancelDuringStream;

    #[async_trait]
    impl LlmProvider for CancelDuringStream {
        fn name(&self) -> &'static str {
            "cancel-during-stream"
        }

        async fn stream(
            &self,
            _request: LlmRequest,
            cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    text: "partial".to_string(),
                }),
                {
                    cancel.cancel();
                    Err(LlmError::transport("Messages request cancelled"))
                },
            ])))
        }
    }

    #[tokio::test]
    async fn a_stop_while_opening_the_stream_is_recorded_as_cancelled() {
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store,
            Arc::new(CancelOnOpen),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(matches!(outcome.outcome, TurnOutcome::Cancelled { .. }));
    }

    #[tokio::test]
    async fn a_provider_error_after_a_stop_is_recorded_as_cancelled() {
        let store = Arc::new(MockStore::default());
        let session_id = SessionId::generate();
        let loop_ = AgentLoop::new(
            store,
            Arc::new(CancelDuringStream),
            tools(),
            config(),
            Arc::new(FixedClock(1_000)),
        )
        .expect("loop");
        let outcome = loop_
            .run(&session_id, "hi", CancellationToken::new(), None)
            .await
            .expect("run");
        assert!(matches!(outcome.outcome, TurnOutcome::Cancelled { .. }));
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
