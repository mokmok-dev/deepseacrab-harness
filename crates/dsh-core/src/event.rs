//! Durable session facts and the projection into model history.

use crate::ids::{EventSeq, StepId, TurnId};
use crate::message::{AssistantMessage, Message, ToolMessage, UserMessage};
use serde::{Deserialize, Serialize};

/// One durable fact in a session, stamped with its log position and wall clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Zero-based position in this session's log.
    pub seq: EventSeq,
    /// Unix epoch milliseconds when the harness accepted the fact.
    pub at: i64,
    /// The fact.
    pub event: SessionEvent,
}

/// Facts appended to a session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The session was created.
    SessionCreated {
        /// Working directory recorded at creation.
        cwd: Option<String>,
        /// Unix epoch milliseconds recorded at creation.
        created_at: i64,
        /// Logical session format version.
        format_version: u32,
    },
    /// One drain of admitted input started.
    TurnStarted {
        /// Turn identity.
        turn_id: TurnId,
    },
    /// One model request plus its tool work started.
    StepStarted {
        /// Owning turn.
        turn_id: TurnId,
        /// Step identity.
        step_id: StepId,
    },
    /// User input was admitted to history.
    UserMessage {
        /// The admitted message.
        message: UserMessage,
    },
    /// A model response was settled.
    AssistantMessage {
        /// The settled message.
        message: AssistantMessage,
    },
    /// A tool result was settled.
    ToolResult {
        /// The settled result.
        message: ToolMessage,
    },
    /// The turn closed.
    TurnEnded {
        /// Turn identity.
        turn_id: TurnId,
        /// Why the turn closed.
        outcome: TurnOutcome,
    },
}

impl SessionEvent {
    /// A stable discriminant used by storage queries and diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::TurnStarted { .. } => "turn_started",
            Self::StepStarted { .. } => "step_started",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::ToolResult { .. } => "tool_result",
            Self::TurnEnded { .. } => "turn_ended",
        }
    }

    /// The message this fact contributes to model history, if any.
    #[must_use]
    pub fn message(&self) -> Option<Message> {
        match self {
            Self::UserMessage { message } => Some(Message::User(message.clone())),
            Self::AssistantMessage { message } => Some(Message::Assistant(message.clone())),
            Self::ToolResult { message } => Some(Message::Tool(message.clone())),
            Self::SessionCreated { .. }
            | Self::TurnStarted { .. }
            | Self::StepStarted { .. }
            | Self::TurnEnded { .. } => None,
        }
    }
}

/// Why a turn closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TurnOutcome {
    /// The model stopped requesting tools.
    Completed,
    /// The caller cancelled the turn.
    Cancelled {
        /// Human-readable cancellation cause.
        reason: String,
    },
    /// A provider, tool, or persistence failure ended the turn.
    Failed {
        /// Stable machine code.
        code: String,
        /// Human-readable detail.
        message: String,
    },
    /// The configured step budget ran out before the model stopped.
    StepLimit,
}

impl TurnOutcome {
    /// Whether the turn achieved its normal terminal state.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// Project the durable log into the ordered model history.
///
/// Lifecycle markers and non-message facts are dropped. The returned order is
/// the order in which the model must read the messages.
#[must_use]
pub fn derive_messages(events: &[EventRecord]) -> Vec<Message> {
    events
        .iter()
        .filter_map(|record| record.event.message())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{MessageId, ToolCallId};
    use crate::message::{AssistantMessage, ContentBlock, TokenUsage, ToolCall};
    use proptest::prelude::*;

    fn user_event(text: String) -> SessionEvent {
        SessionEvent::UserMessage {
            message: UserMessage::text(text),
        }
    }

    fn assistant_event(text: String, call_id: ToolCallId) -> SessionEvent {
        SessionEvent::AssistantMessage {
            message: AssistantMessage {
                id: MessageId::generate(),
                content: vec![
                    ContentBlock::text(text),
                    ContentBlock::tool_call(
                        ToolCall::new(call_id, "bash", r#"{"command":"true"}"#)
                            .expect("valid call"),
                    ),
                ],
                provider: crate::ids::ProviderId::parse("deepseek").expect("provider"),
                model: crate::ids::ModelId::parse("deepseek-flash").expect("model"),
                usage: TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..TokenUsage::default()
                },
            },
        }
    }

    fn event() -> impl Strategy<Value = SessionEvent> {
        prop_oneof![
            any::<String>().prop_map(user_event),
            Just(SessionEvent::TurnStarted {
                turn_id: TurnId::generate()
            }),
            Just(SessionEvent::StepStarted {
                turn_id: TurnId::generate(),
                step_id: StepId::generate(),
            }),
            (any::<String>(), any::<String>()).prop_map(|(_text, reason)| {
                SessionEvent::TurnEnded {
                    turn_id: TurnId::generate(),
                    outcome: if reason.is_empty() {
                        TurnOutcome::Cancelled { reason }
                    } else {
                        TurnOutcome::Completed
                    },
                }
            }),
            any::<String>().prop_map(|text| assistant_event(text, ToolCallId::generate())),
            any::<String>().prop_map(|text| SessionEvent::ToolResult {
                message: ToolMessage::text(ToolCallId::generate(), text),
            }),
        ]
    }

    fn record(event: SessionEvent, seq: u64) -> EventRecord {
        EventRecord {
            seq: EventSeq::new(seq),
            at: 1_700_000_000_000,
            event,
        }
    }

    proptest! {
        #[test]
        fn event_records_round_trip_through_json(event in event(), seq in any::<u64>()) {
            let record = record(event, seq);
            let json = serde_json::to_string(&record).expect("serialize");
            let decoded: EventRecord = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(decoded, record);
        }

        #[test]
        fn projection_keeps_only_messages_in_order(events in prop::collection::vec(event(), 0..32)) {
            let records: Vec<_> = events
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, event)| record(event, u64::try_from(index).expect("fits")))
                .collect();
            let expected: Vec<_> = records
                .iter()
                .filter_map(|record| record.event.message())
                .collect();
            prop_assert_eq!(derive_messages(&records), expected);
        }
    }

    #[test]
    fn lifecycle_markers_are_not_model_history() {
        let records = vec![
            record(
                SessionEvent::SessionCreated {
                    cwd: None,
                    created_at: 5,
                    format_version: 1,
                },
                0,
            ),
            record(
                SessionEvent::TurnStarted {
                    turn_id: TurnId::generate(),
                },
                1,
            ),
            record(user_event("hello".to_string()), 2),
            record(
                SessionEvent::TurnEnded {
                    turn_id: TurnId::generate(),
                    outcome: TurnOutcome::Completed,
                },
                3,
            ),
        ];
        let messages = derive_messages(&records);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "hello");
    }

    #[test]
    fn outcome_success_tracks_completion() {
        assert!(TurnOutcome::Completed.is_success());
        assert!(!TurnOutcome::StepLimit.is_success());
        assert!(
            !TurnOutcome::Cancelled {
                reason: "user".to_string()
            }
            .is_success()
        );
        assert!(
            !TurnOutcome::Failed {
                code: "X".to_string(),
                message: "boom".to_string()
            }
            .is_success()
        );
    }

    #[test]
    fn event_kinds_are_stable() {
        assert_eq!(
            SessionEvent::TurnStarted {
                turn_id: TurnId::generate()
            }
            .kind(),
            "turn_started"
        );
        assert_eq!(
            SessionEvent::ToolResult {
                message: ToolMessage::text(ToolCallId::generate(), "ok")
            }
            .kind(),
            "tool_result"
        );
    }
}
