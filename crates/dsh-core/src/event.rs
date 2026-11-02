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
    /// The session todo list was replaced.
    TodoWritten {
        /// Full replacement list.
        items: Vec<TodoItem>,
    },
    /// The session plan was set or updated.
    PlanSet {
        /// Ordered plan steps.
        steps: Vec<String>,
        /// Whether the user approved the plan.
        approved: bool,
    },
    /// The session goal was set or updated.
    GoalSet {
        /// Completion objective.
        objective: String,
        /// Current phase.
        phase: GoalPhase,
    },
    /// The session working directory changed.
    WorkingDirectoryChanged {
        /// Physical directory.
        cwd: String,
    },
    /// Earlier history was replaced by a durable summary.
    Compaction {
        /// Summary that replaces the earlier messages.
        summary: String,
        /// First retained event position.
        keep_from: EventSeq,
    },
    /// One retry attempt was scheduled for a step.
    RetryAttempt {
        /// Step being retried.
        step_id: StepId,
        /// One-based attempt number.
        attempt: u32,
        /// Machine code of the failure that triggered the retry.
        code: String,
    },
}

/// One item in the session todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Stable item identity.
    pub id: String,
    /// Task text.
    pub text: String,
    /// Current status.
    pub status: TodoStatus,
}

/// Lifecycle of one todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// Lifecycle of the session goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    /// Automatic continuation is armed.
    Active,
    /// Automatic continuation is paused.
    Paused,
    /// Progress needs human input.
    Blocked,
    /// The objective was met.
    Complete,
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
            Self::TodoWritten { .. } => "todo_written",
            Self::PlanSet { .. } => "plan_set",
            Self::GoalSet { .. } => "goal_set",
            Self::WorkingDirectoryChanged { .. } => "working_directory_changed",
            Self::Compaction { .. } => "compaction",
            Self::RetryAttempt { .. } => "retry_attempt",
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
            | Self::TurnEnded { .. }
            | Self::TodoWritten { .. }
            | Self::PlanSet { .. }
            | Self::GoalSet { .. }
            | Self::WorkingDirectoryChanged { .. }
            | Self::Compaction { .. }
            | Self::RetryAttempt { .. } => None,
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
    let compaction = events.iter().rev().find_map(|record| match &record.event {
        SessionEvent::Compaction { summary, keep_from } => Some((summary, *keep_from)),
        _ => None,
    });
    let mut messages = Vec::new();
    if let Some((summary, keep_from)) = compaction {
        messages.push(Message::user(format!(
            "Summary of earlier conversation:\n{summary}"
        )));
        for record in events {
            if record.seq >= keep_from {
                if let Some(message) = record.event.message() {
                    messages.push(message);
                }
            }
        }
    } else {
        for record in events {
            if let Some(message) = record.event.message() {
                messages.push(message);
            }
        }
    }
    messages
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

    #[test]
    fn compaction_replaces_earlier_messages() {
        let records = vec![
            record(
                SessionEvent::SessionCreated {
                    cwd: None,
                    created_at: 0,
                    format_version: 1,
                },
                0,
            ),
            record(user_event("old question".to_string()), 1),
            record(
                SessionEvent::AssistantMessage {
                    message: AssistantMessage {
                        id: MessageId::generate(),
                        content: vec![ContentBlock::text("old answer")],
                        provider: crate::ids::ProviderId::parse("deepseek").expect("provider"),
                        model: crate::ids::ModelId::parse("deepseek-flash").expect("model"),
                        usage: TokenUsage::default(),
                    },
                },
                2,
            ),
            record(
                SessionEvent::Compaction {
                    summary: "we discussed the old question".to_string(),
                    keep_from: EventSeq::new(4),
                },
                3,
            ),
            record(user_event("new question".to_string()), 4),
        ];
        let messages = derive_messages(&records);
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0]
                .text()
                .contains("Summary of earlier conversation")
        );
        assert!(messages[0].text().contains("we discussed the old question"));
        assert_eq!(messages[1].text(), "new question");
    }

    #[test]
    fn session_management_facts_are_not_model_history() {
        let events = [
            SessionEvent::TodoWritten {
                items: vec![TodoItem {
                    id: "1".to_string(),
                    text: "work".to_string(),
                    status: TodoStatus::Pending,
                }],
            },
            SessionEvent::PlanSet {
                steps: vec!["step".to_string()],
                approved: false,
            },
            SessionEvent::GoalSet {
                objective: "ship".to_string(),
                phase: GoalPhase::Active,
            },
            SessionEvent::WorkingDirectoryChanged {
                cwd: "/work".to_string(),
            },
            SessionEvent::RetryAttempt {
                step_id: StepId::generate(),
                attempt: 1,
                code: "RATE_LIMITED".to_string(),
            },
        ];
        for event in events {
            assert!(event.message().is_none());
            assert_ne!(event.kind(), "");
        }
    }
}
