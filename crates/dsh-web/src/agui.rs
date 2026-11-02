//! AG-UI wire events and the mapping from one run's frames.
//!
//! [`AguiEvent`] is the contract a stock `TanStack` AI client parses, so the
//! serialized field names in this module are load-bearing.

use dsh_agent::RunEvent;
use dsh_core::{ToolCallId, TurnOutcome};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    Assistant,
    Reasoning,
    Tool,
}

/// `TanStack`'s own read of a tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
enum TanstackState {
    OutputError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResultMetadata {
    tanstack: TanstackState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all_fields = "camelCase")]
pub enum AguiEvent {
    RunStarted {
        thread_id: String,
        run_id: String,
    },
    TextMessageStart {
        message_id: String,
        role: MessageRole,
    },
    TextMessageContent {
        message_id: String,
        delta: String,
    },
    TextMessageEnd {
        message_id: String,
    },
    ReasoningStart {
        message_id: String,
    },
    ReasoningMessageStart {
        message_id: String,
        role: MessageRole,
    },
    ReasoningMessageContent {
        message_id: String,
        delta: String,
    },
    ReasoningMessageEnd {
        message_id: String,
    },
    ReasoningEnd {
        message_id: String,
    },
    ToolCallStart {
        tool_call_id: ToolCallId,
        tool_call_name: String,
    },
    ToolCallArgs {
        tool_call_id: ToolCallId,
        delta: String,
    },
    ToolCallEnd {
        tool_call_id: ToolCallId,
    },
    ToolCallResult {
        message_id: String,
        tool_call_id: ToolCallId,
        content: String,
        role: MessageRole,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<ToolResultMetadata>,
    },
    RunFinished {
        thread_id: String,
        run_id: String,
    },
    RunError {
        message: String,
        code: String,
    },
}

#[derive(Debug)]
pub struct EventBridge {
    thread_id: String,
    run_id: String,
    text: Option<String>,
    reasoning: Option<String>,
    tools: BTreeSet<ToolCallId>,
    next_id: u64,
    terminated: bool,
}

impl EventBridge {
    #[must_use]
    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            text: None,
            reasoning: None,
            tools: BTreeSet::new(),
            next_id: 0,
            terminated: false,
        }
    }

    /// Map one run frame. Returns nothing once the run reached its terminal event.
    #[must_use]
    pub fn push(&mut self, event: RunEvent) -> Vec<AguiEvent> {
        if self.terminated {
            return Vec::new();
        }
        let mut out = Vec::new();
        match event {
            RunEvent::Text { text } if !text.is_empty() => {
                self.close_reasoning(&mut out);
                let message_id = self.text_message_id(&mut out);
                out.push(AguiEvent::TextMessageContent {
                    message_id,
                    delta: text,
                });
            }
            RunEvent::Thinking { text } if !text.is_empty() => {
                self.close_text(&mut out);
                let message_id = self.reasoning_message_id(&mut out);
                out.push(AguiEvent::ReasoningMessageContent {
                    message_id,
                    delta: text,
                });
            }
            RunEvent::ToolCall { id, name } => {
                self.close_text(&mut out);
                self.close_reasoning(&mut out);
                out.push(AguiEvent::ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_call_name: name,
                });
                self.tools.insert(id);
            }
            RunEvent::ToolCallArgs { id, arguments }
                if self.tools.contains(&id) && !arguments.is_empty() =>
            {
                out.push(AguiEvent::ToolCallArgs {
                    tool_call_id: id,
                    delta: arguments,
                });
            }
            RunEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => {
                self.close_text(&mut out);
                self.close_reasoning(&mut out);
                self.end_tool(&id, &mut out);
                out.push(AguiEvent::ToolCallResult {
                    message_id: self.next_message_id(),
                    tool_call_id: id,
                    content: output,
                    role: MessageRole::Tool,
                    metadata: is_error.then_some(ToolResultMetadata {
                        tanstack: TanstackState::OutputError,
                    }),
                });
            }
            RunEvent::Final { outcome, .. } => {
                self.close_text(&mut out);
                self.close_reasoning(&mut out);
                self.close_tools(&mut out);
                out.push(self.terminal(outcome));
                self.terminated = true;
            }
            RunEvent::Error { code, message } => {
                self.close_text(&mut out);
                self.close_reasoning(&mut out);
                self.close_tools(&mut out);
                out.push(AguiEvent::RunError { message, code });
                self.terminated = true;
            }
            RunEvent::Session { .. }
            | RunEvent::Status { .. }
            | RunEvent::Retry { .. }
            | RunEvent::Text { .. }
            | RunEvent::Thinking { .. }
            | RunEvent::ToolCallArgs { .. } => {}
        }
        out
    }

    fn terminal(&self, outcome: TurnOutcome) -> AguiEvent {
        match outcome {
            TurnOutcome::Completed => AguiEvent::RunFinished {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
            },
            TurnOutcome::Cancelled { reason } => AguiEvent::RunError {
                message: reason,
                code: "CANCELLED".to_string(),
            },
            TurnOutcome::Failed { code, message } => AguiEvent::RunError { message, code },
            TurnOutcome::StepLimit => AguiEvent::RunError {
                message: "step limit reached".to_string(),
                code: "STEP_LIMIT".to_string(),
            },
        }
    }

    fn text_message_id(&mut self, out: &mut Vec<AguiEvent>) -> String {
        if let Some(id) = &self.text {
            return id.clone();
        }
        let message_id = self.next_message_id();
        out.push(AguiEvent::TextMessageStart {
            message_id: message_id.clone(),
            role: MessageRole::Assistant,
        });
        self.text = Some(message_id.clone());
        message_id
    }

    fn reasoning_message_id(&mut self, out: &mut Vec<AguiEvent>) -> String {
        if let Some(id) = &self.reasoning {
            return id.clone();
        }
        let message_id = self.next_message_id();
        out.push(AguiEvent::ReasoningStart {
            message_id: message_id.clone(),
        });
        out.push(AguiEvent::ReasoningMessageStart {
            message_id: message_id.clone(),
            role: MessageRole::Reasoning,
        });
        self.reasoning = Some(message_id.clone());
        message_id
    }

    fn close_text(&mut self, out: &mut Vec<AguiEvent>) {
        if let Some(message_id) = self.text.take() {
            out.push(AguiEvent::TextMessageEnd { message_id });
        }
    }

    fn close_reasoning(&mut self, out: &mut Vec<AguiEvent>) {
        if let Some(message_id) = self.reasoning.take() {
            out.push(AguiEvent::ReasoningMessageEnd {
                message_id: message_id.clone(),
            });
            out.push(AguiEvent::ReasoningEnd { message_id });
        }
    }

    fn end_tool(&mut self, id: &ToolCallId, out: &mut Vec<AguiEvent>) {
        if self.tools.remove(id) {
            out.push(AguiEvent::ToolCallEnd {
                tool_call_id: id.clone(),
            });
        }
    }

    fn close_tools(&mut self, out: &mut Vec<AguiEvent>) {
        for tool_call_id in std::mem::take(&mut self.tools) {
            out.push(AguiEvent::ToolCallEnd { tool_call_id });
        }
    }

    fn next_message_id(&mut self) -> String {
        let id = format!("{}:{}", self.run_id, self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::SessionId;
    use serde_json::{Value, json};

    fn call(id: &str) -> ToolCallId {
        ToolCallId::parse(id).expect("call id")
    }

    fn json_of(event: &AguiEvent) -> Value {
        serde_json::to_value(event).expect("json")
    }

    fn types(events: &[AguiEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| json_of(event)["type"].as_str().expect("type").to_string())
            .collect()
    }

    fn final_event(outcome: TurnOutcome) -> RunEvent {
        RunEvent::Final {
            session_id: SessionId::parse("session-1").expect("id"),
            outcome,
            text: String::new(),
        }
    }

    #[test]
    fn text_opens_streams_then_ends() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let opened = bridge.push(RunEvent::Text {
            text: "hel".to_string(),
        });
        assert_eq!(
            types(&opened),
            ["TEXT_MESSAGE_START", "TEXT_MESSAGE_CONTENT"]
        );
        assert_eq!(json_of(&opened[0])["role"], "assistant");
        assert_eq!(
            json_of(&opened[0])["messageId"],
            json_of(&opened[1])["messageId"]
        );
        assert_eq!(json_of(&opened[1])["delta"], "hel");

        let more = bridge.push(RunEvent::Text {
            text: "lo".to_string(),
        });
        assert_eq!(types(&more), ["TEXT_MESSAGE_CONTENT"]);
        assert_eq!(
            json_of(&more[0])["messageId"],
            json_of(&opened[0])["messageId"]
        );

        let finished = bridge.push(final_event(TurnOutcome::Completed));
        assert_eq!(types(&finished), ["TEXT_MESSAGE_END", "RUN_FINISHED"]);
        assert_eq!(
            json_of(&finished[0])["messageId"],
            json_of(&opened[0])["messageId"]
        );
        assert_eq!(json_of(&finished[1])["threadId"], "session-1");
        assert_eq!(json_of(&finished[1])["runId"], "run-1");
    }

    #[test]
    fn a_text_block_after_a_tool_call_gets_a_new_id() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let before = bridge.push(RunEvent::Text {
            text: "before".to_string(),
        });
        let _ = bridge.push(RunEvent::ToolCall {
            id: call("call-1"),
            name: "echo".to_string(),
        });
        let _ = bridge.push(RunEvent::ToolResult {
            id: call("call-1"),
            name: "echo".to_string(),
            output: "{}".to_string(),
            is_error: false,
        });
        let after = bridge.push(RunEvent::Text {
            text: "after".to_string(),
        });
        assert_eq!(
            types(&after),
            ["TEXT_MESSAGE_START", "TEXT_MESSAGE_CONTENT"]
        );
        assert_ne!(
            json_of(&after[0])["messageId"],
            json_of(&before[0])["messageId"]
        );
    }

    #[test]
    fn reasoning_closes_before_text_starts() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let reasoning = bridge.push(RunEvent::Thinking {
            text: "why".to_string(),
        });
        assert_eq!(
            types(&reasoning),
            [
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT"
            ]
        );
        assert_eq!(json_of(&reasoning[1])["role"], "reasoning");
        assert_eq!(
            json_of(&reasoning[0])["messageId"],
            json_of(&reasoning[2])["messageId"]
        );

        let text = bridge.push(RunEvent::Text {
            text: "hi".to_string(),
        });
        assert_eq!(
            types(&text),
            [
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT"
            ]
        );
        assert_eq!(
            json_of(&text[0])["messageId"],
            json_of(&reasoning[2])["messageId"]
        );
    }

    #[test]
    fn tool_arguments_arrive_in_order_before_the_result() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let start = bridge.push(RunEvent::ToolCall {
            id: call("call-1"),
            name: "echo".to_string(),
        });
        assert_eq!(types(&start), ["TOOL_CALL_START"]);
        assert_eq!(json_of(&start[0])["toolCallId"], "call-1");
        assert_eq!(json_of(&start[0])["toolCallName"], "echo");

        assert_eq!(
            bridge.push(RunEvent::ToolCallArgs {
                id: call("call-1"),
                arguments: String::new(),
            }),
            Vec::<AguiEvent>::new()
        );

        let first = bridge.push(RunEvent::ToolCallArgs {
            id: call("call-1"),
            arguments: r#"{"value""#.to_string(),
        });
        let second = bridge.push(RunEvent::ToolCallArgs {
            id: call("call-1"),
            arguments: ":1}".to_string(),
        });
        assert_eq!(types(&first), ["TOOL_CALL_ARGS"]);
        assert_eq!(json_of(&first[0])["toolCallId"], "call-1");
        assert_eq!(json_of(&first[0])["delta"], r#"{"value""#);
        assert_eq!(types(&second), ["TOOL_CALL_ARGS"]);
        assert_eq!(json_of(&second[0])["delta"], ":1}");

        let result = bridge.push(RunEvent::ToolResult {
            id: call("call-1"),
            name: "echo".to_string(),
            output: "hi".to_string(),
            is_error: false,
        });
        assert_eq!(types(&result), ["TOOL_CALL_END", "TOOL_CALL_RESULT"]);
        assert_eq!(json_of(&result[0])["toolCallId"], "call-1");
        assert_eq!(json_of(&result[1])["toolCallId"], "call-1");
        assert_eq!(json_of(&result[1])["role"], "tool");
        assert_eq!(json_of(&result[1])["content"], "hi");
        assert!(json_of(&result[1]).get("metadata").is_none());
    }

    #[test]
    fn arguments_for_an_unopened_call_are_dropped() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        assert_eq!(
            bridge.push(RunEvent::ToolCallArgs {
                id: call("call-1"),
                arguments: r#"{"value""#.to_string(),
            }),
            Vec::<AguiEvent>::new()
        );
    }

    #[test]
    fn an_error_result_carries_the_tanstack_state() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let _ = bridge.push(RunEvent::ToolCall {
            id: call("call-1"),
            name: "echo".to_string(),
        });
        let result = bridge.push(RunEvent::ToolResult {
            id: call("call-1"),
            name: "echo".to_string(),
            output: "no such tool".to_string(),
            is_error: true,
        });
        assert_eq!(
            json_of(&result[1])["metadata"],
            json!({"tanstack": {"state": "output-error"}})
        );
    }

    #[test]
    fn completed_emits_one_run_finished() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let finished = bridge.push(final_event(TurnOutcome::Completed));
        assert_eq!(types(&finished), ["RUN_FINISHED"]);
        assert_eq!(
            bridge.push(final_event(TurnOutcome::Completed)),
            Vec::<AguiEvent>::new()
        );
    }

    #[test]
    fn an_open_tool_call_closes_with_the_run() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let _ = bridge.push(RunEvent::ToolCall {
            id: call("call-1"),
            name: "echo".to_string(),
        });
        let finished = bridge.push(final_event(TurnOutcome::Completed));
        assert_eq!(types(&finished), ["TOOL_CALL_END", "RUN_FINISHED"]);
        assert_eq!(json_of(&finished[0])["toolCallId"], "call-1");
    }

    #[test]
    fn an_error_suppresses_the_final_event() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        let error = bridge.push(RunEvent::Error {
            code: "STORE_ERROR".to_string(),
            message: "disk full".to_string(),
        });
        assert_eq!(types(&error), ["RUN_ERROR"]);
        assert_eq!(json_of(&error[0])["code"], "STORE_ERROR");
        assert_eq!(json_of(&error[0])["message"], "disk full");
        assert_eq!(
            bridge.push(final_event(TurnOutcome::Completed)),
            Vec::<AguiEvent>::new()
        );
    }

    #[test]
    fn non_completed_outcomes_map_to_stable_codes() {
        let cases = [
            (
                TurnOutcome::Cancelled {
                    reason: "user stopped".to_string(),
                },
                "CANCELLED",
                "user stopped",
            ),
            (
                TurnOutcome::Failed {
                    code: "SERVER_ERROR".to_string(),
                    message: "boom".to_string(),
                },
                "SERVER_ERROR",
                "boom",
            ),
            (TurnOutcome::StepLimit, "STEP_LIMIT", "step limit reached"),
        ];
        for (outcome, code, message) in cases {
            let mut bridge = EventBridge::new("session-1", "run-1");
            let events = bridge.push(final_event(outcome));
            assert_eq!(types(&events), ["RUN_ERROR"]);
            assert_eq!(json_of(&events[0])["code"], code);
            assert_eq!(json_of(&events[0])["message"], message);
        }
    }

    #[test]
    fn idle_events_emit_nothing() {
        let mut bridge = EventBridge::new("session-1", "run-1");
        for event in [
            RunEvent::Session {
                session_id: SessionId::parse("session-1").expect("id"),
            },
            RunEvent::Status {
                text: "working".to_string(),
            },
            RunEvent::Retry {
                attempt: 1,
                code: "RATE_LIMITED".to_string(),
            },
            RunEvent::ToolCallArgs {
                id: call("call-1"),
                arguments: String::new(),
            },
            RunEvent::Text {
                text: String::new(),
            },
            RunEvent::Thinking {
                text: String::new(),
            },
        ] {
            assert_eq!(bridge.push(event), Vec::<AguiEvent>::new());
        }
        assert_eq!(
            types(&bridge.push(final_event(TurnOutcome::Completed))),
            ["RUN_FINISHED"]
        );
    }
}
