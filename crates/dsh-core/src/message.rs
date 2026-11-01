//! Provider-neutral messages and content blocks.

use crate::ids::{MessageId, ModelId, ProviderId, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// A model invocation requested by the assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-issued call identity used to pair the result.
    pub id: ToolCallId,
    /// Registered tool name.
    pub name: String,
    /// Raw JSON argument text exactly as the model produced it.
    pub arguments: String,
}

impl ToolCall {
    /// Build a tool call, rejecting an empty tool name.
    pub fn new(
        id: ToolCallId,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Result<Self, MessageError> {
        let name = name.into();
        if name.is_empty() {
            return Err(MessageError::EmptyToolName);
        }
        Ok(Self {
            id,
            name,
            arguments: arguments.into(),
        })
    }

    /// Parse the raw argument text as a JSON object.
    pub fn arguments_value(&self) -> Result<Value, MessageError> {
        let value: Value = serde_json::from_str(&self.arguments)
            .map_err(|error| MessageError::InvalidArguments(error.to_string()))?;
        if value.is_object() {
            Ok(value)
        } else {
            Err(MessageError::NonObjectArguments)
        }
    }
}

/// One model-facing content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Visible output text.
    Text {
        /// The text.
        text: String,
    },
    /// Provider reasoning, retained for replay.
    Reasoning {
        /// The reasoning text.
        text: String,
    },
    /// A requested tool invocation.
    ToolCall {
        /// The invocation.
        call: ToolCall,
    },
}

impl ContentBlock {
    /// Build a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Build a reasoning block.
    #[must_use]
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self::Reasoning { text: text.into() }
    }

    /// Build a tool-call block.
    #[must_use]
    pub const fn tool_call(call: ToolCall) -> Self {
        Self::ToolCall { call }
    }

    /// The block's text when it carries text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } | Self::Reasoning { text } => Some(text),
            Self::ToolCall { .. } => None,
        }
    }
}

/// Rejection raised while constructing a message or parsing its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MessageError {
    /// A tool call carried no tool name.
    #[error("tool call name must not be empty")]
    EmptyToolName,
    /// Raw tool arguments were not JSON.
    #[error("tool arguments are not valid JSON: {0}")]
    InvalidArguments(String),
    /// Raw tool arguments were JSON but not an object.
    #[error("tool arguments must be a JSON object")]
    NonObjectArguments,
}

/// A rendered system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemMessage {
    /// Stable message identity.
    pub id: MessageId,
    /// Prompt text.
    pub text: String,
}

/// Tool availability changes sent to the model between requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeveloperMessage {
    /// Stable message identity.
    pub id: MessageId,
    /// Development note text.
    pub text: String,
}

/// Input supplied by the harness user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Stable message identity.
    pub id: MessageId,
    /// User content.
    pub content: Vec<ContentBlock>,
}

impl UserMessage {
    /// Build a user message from plain text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            id: MessageId::generate(),
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Concatenate visible text blocks.
    #[must_use]
    pub fn text_content(&self) -> String {
        join_text(&self.content)
    }
}

/// Token accounting for one model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens read from the provider cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the provider cache.
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    /// The full billable total.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Accumulate another call's usage into this one.
    pub const fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }
}

/// A model response retained in durable history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Stable message identity.
    pub id: MessageId,
    /// Visible output, reasoning, and requested tool calls in provider order.
    pub content: Vec<ContentBlock>,
    /// Route that produced the message.
    pub provider: ProviderId,
    /// Model that produced the message.
    pub model: ModelId,
    /// Reported usage for the call.
    pub usage: TokenUsage,
}

impl AssistantMessage {
    /// Build an assistant message.
    #[must_use]
    pub fn new(
        content: Vec<ContentBlock>,
        provider: ProviderId,
        model: ModelId,
        usage: TokenUsage,
    ) -> Self {
        Self {
            id: MessageId::generate(),
            content,
            provider,
            model,
            usage,
        }
    }

    /// Concatenate visible text blocks.
    #[must_use]
    pub fn text(&self) -> String {
        join_text(&self.content)
    }

    /// Every tool call carried by this message.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call } => Some(call),
                ContentBlock::Text { .. } | ContentBlock::Reasoning { .. } => None,
            })
            .collect()
    }
}

/// Result of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMessage {
    /// Stable message identity.
    pub id: MessageId,
    /// Call this result answers.
    pub call_id: ToolCallId,
    /// Result content.
    pub content: Vec<ContentBlock>,
    /// Whether the invocation failed.
    pub is_error: bool,
}

impl ToolMessage {
    /// Build a successful text result.
    #[must_use]
    pub fn text(call_id: ToolCallId, text: impl Into<String>) -> Self {
        Self {
            id: MessageId::generate(),
            call_id,
            content: vec![ContentBlock::text(text)],
            is_error: false,
        }
    }

    /// Build a failed text result.
    #[must_use]
    pub fn error(call_id: ToolCallId, text: impl Into<String>) -> Self {
        Self {
            id: MessageId::generate(),
            call_id,
            content: vec![ContentBlock::text(text)],
            is_error: true,
        }
    }

    /// Concatenate visible text blocks.
    #[must_use]
    pub fn text_content(&self) -> String {
        join_text(&self.content)
    }
}

/// Any durable conversation message, tagged by role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Rendered system prompt.
    System(SystemMessage),
    /// Tool availability note.
    Developer(DeveloperMessage),
    /// Harness user input.
    User(UserMessage),
    /// Model output.
    Assistant(AssistantMessage),
    /// Tool result.
    Tool(ToolMessage),
}

impl Message {
    /// Build a system message.
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self::System(SystemMessage {
            id: MessageId::generate(),
            text: text.into(),
        })
    }

    /// Build a user message from plain text.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserMessage::text(text))
    }

    /// Build an assistant message.
    #[must_use]
    pub fn assistant(
        content: Vec<ContentBlock>,
        provider: ProviderId,
        model: ModelId,
        usage: TokenUsage,
    ) -> Self {
        Self::Assistant(AssistantMessage::new(content, provider, model, usage))
    }

    /// Build a successful tool result.
    #[must_use]
    pub fn tool_text(call_id: ToolCallId, text: impl Into<String>) -> Self {
        Self::Tool(ToolMessage::text(call_id, text))
    }

    /// Build a failed tool result.
    #[must_use]
    pub fn tool_error(call_id: ToolCallId, text: impl Into<String>) -> Self {
        Self::Tool(ToolMessage::error(call_id, text))
    }

    /// Stable identity of this message.
    #[must_use]
    pub const fn id(&self) -> &MessageId {
        match self {
            Self::System(message) => &message.id,
            Self::Developer(message) => &message.id,
            Self::User(message) => &message.id,
            Self::Assistant(message) => &message.id,
            Self::Tool(message) => &message.id,
        }
    }

    /// The message role.
    #[must_use]
    pub const fn role(&self) -> Role {
        match self {
            Self::System(_) => Role::System,
            Self::Developer(_) => Role::Developer,
            Self::User(_) => Role::User,
            Self::Assistant(_) => Role::Assistant,
            Self::Tool(_) => Role::Tool,
        }
    }

    /// Concatenate visible text blocks. System and developer messages own text directly.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::System(message) => message.text.clone(),
            Self::Developer(message) => message.text.clone(),
            Self::User(message) => join_text(&message.content),
            Self::Assistant(message) => join_text(&message.content),
            Self::Tool(message) => join_text(&message.content),
        }
    }

    /// Every tool call carried by an assistant message.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        match self {
            Self::Assistant(message) => message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { call } => Some(call),
                    ContentBlock::Text { .. } | ContentBlock::Reasoning { .. } => None,
                })
                .collect(),
            Self::System(_) | Self::Developer(_) | Self::User(_) | Self::Tool(_) => Vec::new(),
        }
    }
}

fn join_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => None,
        })
        .collect()
}

/// Role of a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System prompt.
    System,
    /// Developer note.
    Developer,
    /// User input.
    User,
    /// Assistant output.
    Assistant,
    /// Tool result.
    Tool,
}

impl Role {
    /// Wire spelling used by JSON and the session log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tool_call() -> impl Strategy<Value = ToolCall> {
        (
            "[A-Za-z0-9_-]{1,32}",
            "[a-z_]{1,16}",
            prop_oneof![
                Just("{}".to_string()),
                Just(r#"{"path":"a.txt"}"#.to_string()),
                Just(r#"{"count":3}"#.to_string()),
            ],
        )
            .prop_map(|(id, name, arguments)| {
                ToolCall::new(ToolCallId::parse(id).expect("valid id"), name, arguments)
                    .expect("valid call")
            })
    }

    fn message() -> impl Strategy<Value = Message> {
        prop_oneof![
            any::<String>().prop_map(Message::system),
            any::<String>().prop_map(Message::user),
            (any::<String>(), tool_call()).prop_map(|(text, call)| {
                Message::assistant(
                    vec![ContentBlock::text(text), ContentBlock::tool_call(call)],
                    ProviderId::parse("deepseek").expect("provider"),
                    ModelId::parse("deepseek-flash").expect("model"),
                    TokenUsage::default(),
                )
            }),
            (any::<String>(), tool_call())
                .prop_map(|(text, call)| { Message::tool_text(call.id, text) }),
        ]
    }

    fn usage() -> impl Strategy<Value = TokenUsage> {
        (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
            |(input_tokens, output_tokens, cache_read_tokens, cache_write_tokens)| TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            },
        )
    }

    fn json_scalar() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("true".to_string()),
            Just("false".to_string()),
            Just("null".to_string()),
            (0u16..10_000).prop_map(|value| value.to_string()),
        ]
    }

    proptest! {
        #[test]
        fn messages_round_trip_through_json(message in message()) {
            let json = serde_json::to_string(&message).expect("serialize");
            let decoded: Message = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(decoded, message);
        }

        #[test]
        fn assistant_text_excludes_reasoning_and_calls(text in any::<String>(), call in tool_call()) {
            let message = Message::assistant(
                vec![
                    ContentBlock::text(text.clone()),
                    ContentBlock::reasoning("hidden"),
                    ContentBlock::tool_call(call),
                ],
                ProviderId::parse("deepseek").expect("provider"),
                ModelId::parse("deepseek-flash").expect("model"),
                TokenUsage::default(),
            );
            prop_assert_eq!(message.text(), text);
            prop_assert_eq!(message.tool_calls().len(), 1);
        }

        #[test]
        fn usage_total_is_the_sum_of_all_fields(
            input in any::<u64>(),
            output in any::<u64>(),
            read in any::<u64>(),
            write in any::<u64>(),
        ) {
            let usage = TokenUsage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: read,
                cache_write_tokens: write,
            };
            prop_assert_eq!(
                usage.total(),
                input.saturating_add(output).saturating_add(read).saturating_add(write)
            );
            prop_assert_eq!(TokenUsage::default().total(), 0);
        }

        #[test]
        fn usage_accumulation_adds_each_field(left in usage(), right in usage()) {
            let mut total = left;
            total.add(right);
            prop_assert_eq!(total.input_tokens, left.input_tokens.saturating_add(right.input_tokens));
            prop_assert_eq!(total.output_tokens, left.output_tokens.saturating_add(right.output_tokens));
            prop_assert_eq!(total.cache_read_tokens, left.cache_read_tokens.saturating_add(right.cache_read_tokens));
            prop_assert_eq!(total.cache_write_tokens, left.cache_write_tokens.saturating_add(right.cache_write_tokens));
        }

        #[test]
        fn object_arguments_parse(key in "[a-z]{1,8}", value in json_scalar()) {
            let raw = format!(r#"{{"{key}":{value}}}"#);
            let call = ToolCall::new(
                ToolCallId::parse("call-1").expect("id"),
                "tool",
                raw.clone(),
            ).expect("call");
            prop_assert_eq!(
                call.arguments_value().expect("object"),
                serde_json::from_str::<serde_json::Value>(&raw).expect("valid json")
            );
        }
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        for raw in ["[]", "3", "null", "\"text\"", "true"] {
            let call =
                ToolCall::new(ToolCallId::parse("call-1").expect("id"), "tool", raw).expect("call");
            assert_eq!(
                call.arguments_value(),
                Err(MessageError::NonObjectArguments)
            );
        }
    }

    #[test]
    fn malformed_arguments_are_rejected() {
        let call =
            ToolCall::new(ToolCallId::parse("call-1").expect("id"), "tool", "{").expect("call");
        assert!(matches!(
            call.arguments_value(),
            Err(MessageError::InvalidArguments(_))
        ));
    }

    #[test]
    fn empty_tool_names_are_rejected() {
        assert_eq!(
            ToolCall::new(ToolCallId::parse("call-1").expect("id"), "", "{}"),
            Err(MessageError::EmptyToolName)
        );
    }

    #[test]
    fn message_roles_match_their_variant() {
        assert_eq!(Message::system("s").role(), Role::System);
        assert_eq!(Message::user("u").role(), Role::User);
        assert_eq!(
            Message::tool_text(ToolCallId::generate(), "t").role(),
            Role::Tool
        );
        assert_eq!(Role::Assistant.as_str(), "assistant");
    }

    #[test]
    fn content_block_constructors_and_text_access() {
        assert_eq!(
            ContentBlock::text("hello"),
            ContentBlock::Text {
                text: "hello".to_string()
            }
        );
        assert_eq!(
            ContentBlock::reasoning("why"),
            ContentBlock::Reasoning {
                text: "why".to_string()
            }
        );
        let call =
            ToolCall::new(ToolCallId::parse("call-1").expect("id"), "tool", "{}").expect("call");
        assert_eq!(
            ContentBlock::tool_call(call.clone()),
            ContentBlock::ToolCall { call }
        );
        assert_eq!(ContentBlock::text("hello").as_text(), Some("hello"));
        assert_eq!(ContentBlock::reasoning("why").as_text(), Some("why"));
        assert_eq!(
            ContentBlock::tool_call(
                ToolCall::new(ToolCallId::parse("call-1").expect("id"), "tool", "{}")
                    .expect("call")
            )
            .as_text(),
            None
        );
    }

    #[test]
    fn message_text_accessors_agree_with_content() {
        let assistant = AssistantMessage::new(
            vec![
                ContentBlock::text("answer"),
                ContentBlock::reasoning("hidden"),
                ContentBlock::tool_call(
                    ToolCall::new(ToolCallId::parse("call-1").expect("id"), "bash", "{}")
                        .expect("call"),
                ),
            ],
            ProviderId::parse("deepseek").expect("provider"),
            ModelId::parse("deepseek-flash").expect("model"),
            TokenUsage::default(),
        );
        assert_eq!(assistant.text(), "answer");
        assert_eq!(assistant.tool_calls().len(), 1);

        let user = UserMessage::text("hello");
        assert_eq!(user.text_content(), "hello");

        let tool = ToolMessage::text(ToolCallId::parse("call-1").expect("id"), "result");
        assert_eq!(tool.text_content(), "result");
        let failure = ToolMessage::error(ToolCallId::parse("call-1").expect("id"), "broken");
        assert!(failure.is_error);
        assert_eq!(failure.text_content(), "broken");
    }
}
