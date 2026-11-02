//! Wire types and pure request/stream translation for the Messages API.

use crate::sse::SseEvent;
use dsh_core::{
    ContentBlock, FinishReason, LlmError, LlmRequest, Message, ReasoningEffort, StreamEvent,
    TokenUsage, ToolCallId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A Messages API request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRequest {
    /// Requested model.
    pub model: String,
    /// Streaming is the only supported mode.
    pub stream: bool,
    /// Output cap.
    pub max_tokens: u32,
    /// Serialized conversation turns.
    pub messages: Vec<WireMessage>,
    /// Top-level system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Thinking policy.
    pub thinking: WireThinking,
    /// Reasoning effort when thinking is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Declared tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
}

/// One serialized conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireMessage {
    /// Wire role.
    pub role: WireRole,
    /// Ordered content blocks.
    pub content: Vec<WireBlock>,
}

/// Roles accepted by the Messages conversation array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireRole {
    /// User input and tool results.
    User,
    /// Model output.
    Assistant,
}

/// One content block in a conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireBlock {
    /// Visible text.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning text.
    Thinking {
        /// The reasoning.
        thinking: String,
    },
    /// A requested tool invocation.
    ToolUse {
        /// Provider-issued call identity.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed argument object.
        input: Value,
    },
    /// A tool result answering an earlier call.
    ToolResult {
        /// Answered call identity.
        tool_use_id: String,
        /// Result content.
        content: Vec<WireInput>,
        /// Whether the invocation failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// An inline image in a user turn.
    Image {
        /// Encoded image source.
        source: WireImageSource,
    },
}

/// An inline base64 image source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireImageSource {
    /// Source discriminator.
    #[serde(rename = "type")]
    pub kind: String,
    /// MIME type.
    pub media_type: String,
    /// Base64 bytes.
    pub data: String,
}

impl WireImageSource {
    /// Build a base64 source.
    #[must_use]
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            kind: "base64".to_string(),
            media_type: media_type.into(),
            data: data.into(),
        }
    }
}

/// Input accepted inside a user turn or tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireInput {
    /// Visible input text.
    Text {
        /// The text.
        text: String,
    },
    /// Inline image bytes.
    Image {
        /// Encoded image source.
        source: WireImageSource,
    },
}

/// Declaration of one tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    /// Registered name.
    pub name: String,
    /// When and how to call it.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub input_schema: Value,
}

/// Thinking enablement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireThinking {
    /// Thinking is enabled.
    Enabled,
    /// Thinking is disabled.
    Disabled,
}

/// Per-request reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    /// Effort spelling.
    pub effort: WireEffort,
}

/// Effort spellings accepted by `output_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEffort {
    /// Little reasoning.
    Low,
    /// Normal reasoning.
    High,
    /// Maximum reasoning.
    Max,
}

/// Serialize one provider-neutral request into a Messages API body.
pub fn serialize_request(request: &LlmRequest) -> Result<WireRequest, LlmError> {
    request.validate()?;
    let messages = build_messages(&request.messages);
    validate_pairing(&messages)?;
    Ok(WireRequest {
        model: request.model.as_str().to_string(),
        stream: true,
        max_tokens: request.max_tokens,
        messages,
        system: system_prompt(request),
        thinking: if request.effort.is_enabled() {
            WireThinking::Enabled
        } else {
            WireThinking::Disabled
        },
        output_config: output_config(request.effort),
        temperature: request.temperature,
        stop_sequences: (!request.stop.is_empty()).then(|| request.stop.clone()),
        tools: (!request.tools.is_empty()).then(|| {
            request
                .tools
                .iter()
                .map(|tool| WireTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                })
                .collect()
        }),
    })
}

const fn output_config(effort: ReasoningEffort) -> Option<OutputConfig> {
    let effort = match effort {
        ReasoningEffort::Off => return None,
        ReasoningEffort::Low => WireEffort::Low,
        ReasoningEffort::High => WireEffort::High,
        ReasoningEffort::Max => WireEffort::Max,
    };
    Some(OutputConfig { effort })
}

fn system_prompt(request: &LlmRequest) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(system) = request.system.as_deref().filter(|text| !text.is_empty()) {
        parts.push(system);
    }
    let mut latest_system = None;
    let mut developer = Vec::new();
    for message in &request.messages {
        match message {
            Message::System(system) if !system.text.is_empty() => {
                latest_system = Some(system.text.as_str());
            }
            Message::Developer(note) if !note.text.is_empty() => developer.push(note.text.as_str()),
            Message::System(_)
            | Message::Developer(_)
            | Message::User(_)
            | Message::Assistant(_)
            | Message::Tool(_) => {}
        }
    }
    parts.extend(latest_system);
    parts.extend(developer);
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn build_messages(history: &[Message]) -> Vec<WireMessage> {
    let mut messages: Vec<WireMessage> = Vec::new();
    for message in history {
        let (role, blocks) = match message {
            Message::System(_) | Message::Developer(_) => continue,
            Message::User(user) => (
                WireRole::User,
                user.content.iter().filter_map(user_block).collect(),
            ),
            Message::Assistant(assistant) => (
                WireRole::Assistant,
                assistant
                    .content
                    .iter()
                    .filter_map(assistant_block)
                    .collect(),
            ),
            Message::Tool(tool) => (WireRole::User, vec![tool_result_block(tool)]),
        };
        if blocks.is_empty() && !matches!(message, Message::Tool(_)) {
            continue;
        }
        if let Some(previous) = messages.last_mut() {
            if previous.role == role {
                previous.content.extend(blocks);
                continue;
            }
        }
        messages.push(WireMessage {
            role,
            content: blocks,
        });
    }
    for message in &mut messages {
        message.content.sort_by_key(|block| match block {
            WireBlock::ToolResult { .. } => 0,
            WireBlock::Text { .. }
            | WireBlock::Thinking { .. }
            | WireBlock::ToolUse { .. }
            | WireBlock::Image { .. } => 1,
        });
    }
    messages
}

fn user_block(block: &ContentBlock) -> Option<WireBlock> {
    match block {
        ContentBlock::Text { text } if !text.is_empty() => {
            Some(WireBlock::Text { text: text.clone() })
        }
        ContentBlock::Text { .. }
        | ContentBlock::Reasoning { .. }
        | ContentBlock::ToolCall { .. } => None,
        ContentBlock::Image { media_type, data } => Some(WireBlock::Image {
            source: WireImageSource::base64(media_type.clone(), data.clone()),
        }),
    }
}

fn assistant_block(block: &ContentBlock) -> Option<WireBlock> {
    match block {
        ContentBlock::Text { text } if !text.is_empty() => {
            Some(WireBlock::Text { text: text.clone() })
        }
        ContentBlock::Text { .. } | ContentBlock::Image { .. } => None,
        ContentBlock::Reasoning { text } => Some(WireBlock::Thinking {
            thinking: text.clone(),
        }),
        ContentBlock::ToolCall { call } => Some(WireBlock::ToolUse {
            id: call.id.as_str().to_string(),
            name: call.name.clone(),
            input: tool_input(&call.arguments),
        }),
    }
}

fn tool_result_block(tool: &dsh_core::ToolMessage) -> WireBlock {
    let content = tool
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                Some(WireInput::Text { text: text.clone() })
            }
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolCall { .. } => None,
            ContentBlock::Image { media_type, data } => Some(WireInput::Image {
                source: WireImageSource::base64(media_type.clone(), data.clone()),
            }),
        })
        .collect();
    WireBlock::ToolResult {
        tool_use_id: tool.call_id.as_str().to_string(),
        content,
        is_error: Some(tool.is_error),
    }
}

fn tool_input(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn validate_pairing(messages: &[WireMessage]) -> Result<(), LlmError> {
    let mut pending: Option<BTreeSet<&str>> = None;
    for message in messages {
        match message.role {
            WireRole::Assistant => {
                let mut calls = BTreeSet::new();
                let mut count = 0_usize;
                for block in &message.content {
                    if let WireBlock::ToolUse { id, .. } = block {
                        calls.insert(id.as_str());
                        count += 1;
                    }
                }
                if calls.len() != count {
                    return Err(LlmError::invalid_request("duplicate tool call id"));
                }
                pending = Some(calls);
            }
            WireRole::User => {
                let mut results = message.content.iter().filter_map(|block| match block {
                    WireBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    WireBlock::Text { .. }
                    | WireBlock::Thinking { .. }
                    | WireBlock::ToolUse { .. }
                    | WireBlock::Image { .. } => None,
                });
                match pending.take() {
                    Some(mut calls) => {
                        for result in results {
                            if !calls.remove(result) {
                                return Err(LlmError::invalid_request(
                                    "tool result has no matching call",
                                ));
                            }
                        }
                        if !calls.is_empty() {
                            return Err(LlmError::invalid_request(
                                "tool calls need immediate results",
                            ));
                        }
                    }
                    None => {
                        if results.next().is_some() {
                            return Err(LlmError::invalid_request(
                                "tool result has no matching call",
                            ));
                        }
                    }
                }
            }
        }
    }
    if pending.is_some_and(|calls| !calls.is_empty()) {
        return Err(LlmError::invalid_request(
            "history ends with unresolved tool calls",
        ));
    }
    Ok(())
}

/// Incremental translator from decoded SSE events to provider-neutral stream
/// events.
#[derive(Debug, Default)]
pub struct Translator {
    started: bool,
    finished: bool,
    blocks: BTreeMap<u64, BlockState>,
    usage: TokenUsage,
    stop_reason: Option<FinishReason>,
}

#[derive(Debug)]
struct BlockState {
    kind: BlockKind,
    closed: bool,
}

#[derive(Debug)]
enum BlockKind {
    Text,
    Thinking,
    ToolCall { id: ToolCallId },
}

impl Translator {
    /// Create a translator positioned before `message_start`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            started: false,
            finished: false,
            blocks: BTreeMap::new(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            stop_reason: None,
        }
    }

    /// Whether `message_stop` has settled the stream.
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    /// Reject a stream that ended before `message_stop`.
    pub fn ensure_finished(&self) -> Result<(), LlmError> {
        if self.finished {
            Ok(())
        } else {
            Err(LlmError::stream_closed(
                "Messages stream ended before message_stop",
            ))
        }
    }

    /// Consume one decoded SSE event and return the events it settles.
    pub fn push(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, LlmError> {
        if self.finished {
            return Ok(Vec::new());
        }
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|_| LlmError::malformed("Messages SSE data is not valid JSON"))?;
        let object = data
            .as_object()
            .ok_or_else(|| malformed("expected a JSON object"))?;
        let event_type = required_str(object, "type")?;
        if let Some(name) = event.event.as_deref() {
            if name != event_type {
                return Err(malformed("SSE event name does not match its JSON type"));
            }
        }
        match event_type {
            "error" => Err(in_band_error(object)),
            "message_start" => self.message_start(object),
            "content_block_start" => self.content_block_start(object),
            "content_block_delta" => self.content_block_delta(object),
            "content_block_stop" => self.content_block_stop(object),
            "message_delta" => self.message_delta(object),
            "message_stop" => self.message_stop(),
            _ => Ok(Vec::new()),
        }
    }

    fn message_start(&mut self, event: &Map<String, Value>) -> Result<Vec<StreamEvent>, LlmError> {
        if self.started {
            return Err(malformed("duplicate message_start"));
        }
        if let Some(message) = event.get("message") {
            let message = message
                .as_object()
                .ok_or_else(|| malformed("message_start message is not an object"))?;
            if let Some(usage) = message.get("usage") {
                update_usage(&mut self.usage, usage)?;
            }
        }
        self.started = true;
        Ok(Vec::new())
    }

    fn content_block_start(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<StreamEvent>, LlmError> {
        self.require_started()?;
        if self.stop_reason.is_some() {
            return Err(malformed("content block starts after the message settled"));
        }
        let index = block_index(event)?;
        if self.blocks.contains_key(&index) {
            return Err(malformed("duplicate content block start"));
        }
        let block = event
            .get("content_block")
            .and_then(Value::as_object)
            .ok_or_else(|| malformed("content_block_start has no content_block object"))?;
        let block_type = required_str(block, "type")?;
        let mut events = Vec::new();
        let kind = match block_type {
            "text" => {
                let text = required_str(block, "text")?;
                if !text.is_empty() {
                    events.push(StreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
                BlockKind::Text
            }
            "thinking" => {
                let thinking = required_str(block, "thinking")?;
                if !thinking.is_empty() {
                    events.push(StreamEvent::ReasoningDelta {
                        text: thinking.to_string(),
                    });
                }
                BlockKind::Thinking
            }
            "tool_use" => {
                let id = ToolCallId::parse(required_str(block, "id")?)
                    .map_err(|_| malformed("invalid tool call id"))?;
                let name = required_str(block, "name")?;
                if name.is_empty() {
                    return Err(malformed("empty tool name"));
                }
                events.push(StreamEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.to_string(),
                });
                BlockKind::ToolCall { id }
            }
            other => {
                return Err(LlmError::unsupported(format!(
                    "Messages cannot represent response block {other}"
                )));
            }
        };
        self.blocks.insert(
            index,
            BlockState {
                kind,
                closed: false,
            },
        );
        Ok(events)
    }

    fn content_block_delta(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<StreamEvent>, LlmError> {
        self.require_started()?;
        let index = block_index(event)?;
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| malformed("content block delta without an open block"))?;
        if block.closed {
            return Err(malformed("content block delta after block stop"));
        }
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| malformed("content_block_delta has no delta object"))?;
        let delta_type = required_str(delta, "type")?;
        let translated = match (delta_type, &block.kind) {
            ("text_delta", BlockKind::Text) => StreamEvent::TextDelta {
                text: required_str(delta, "text")?.to_string(),
            },
            ("thinking_delta", BlockKind::Thinking) => StreamEvent::ReasoningDelta {
                text: required_str(delta, "thinking")?.to_string(),
            },
            ("input_json_delta", BlockKind::ToolCall { id }) => StreamEvent::ToolCallDelta {
                id: id.clone(),
                arguments: required_str(delta, "partial_json")?.to_string(),
            },
            ("signature_delta", BlockKind::Thinking) => {
                required_str(delta, "signature")?;
                return Ok(Vec::new());
            }
            (other, _) => {
                return Err(malformed(format!(
                    "unsupported content block delta {other}"
                )));
            }
        };
        Ok(vec![translated])
    }

    fn content_block_stop(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<StreamEvent>, LlmError> {
        self.require_started()?;
        let index = block_index(event)?;
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| malformed("content block stop without an open block"))?;
        if block.closed {
            return Err(malformed("duplicate content block stop"));
        }
        block.closed = true;
        Ok(Vec::new())
    }

    fn message_delta(&mut self, event: &Map<String, Value>) -> Result<Vec<StreamEvent>, LlmError> {
        self.require_started()?;
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| malformed("message_delta has no delta object"))?;
        if let Some(reason) = delta.get("stop_reason") {
            if !reason.is_null() {
                let raw = reason
                    .as_str()
                    .ok_or_else(|| malformed("stop_reason is not a string"))?;
                self.stop_reason = Some(
                    FinishReason::from_wire(raw)
                        .ok_or_else(|| malformed(format!("unsupported stop reason {raw}")))?,
                );
            }
        }
        if let Some(usage) = event.get("usage") {
            update_usage(&mut self.usage, usage)?;
        }
        Ok(Vec::new())
    }

    fn message_stop(&mut self) -> Result<Vec<StreamEvent>, LlmError> {
        self.require_started()?;
        let reason = self
            .stop_reason
            .ok_or_else(|| malformed("message_stop without a stop reason"))?;
        if self.blocks.values().any(|block| !block.closed) {
            return Err(malformed("message_stop with open content blocks"));
        }
        if self.blocks.is_empty() && reason == FinishReason::Stop {
            return Err(LlmError::empty_response("Messages returned no content"));
        }
        self.finished = true;
        Ok(vec![
            StreamEvent::Usage { usage: self.usage },
            StreamEvent::Finish { reason },
        ])
    }

    fn require_started(&self) -> Result<(), LlmError> {
        if self.started {
            Ok(())
        } else {
            Err(malformed("event before message_start"))
        }
    }
}

fn malformed(detail: impl Into<String>) -> LlmError {
    LlmError::malformed(format!("Messages stream: {}", detail.into()))
}

fn required_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, LlmError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(format!("expected a string field {key}")))
}

fn block_index(event: &Map<String, Value>) -> Result<u64, LlmError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed("invalid content block index"))
}

fn update_usage(usage: &mut TokenUsage, raw: &Value) -> Result<(), LlmError> {
    let fields = raw
        .as_object()
        .ok_or_else(|| malformed("usage is not an object"))?;
    let targets: [(&str, &mut u64); 4] = [
        ("input_tokens", &mut usage.input_tokens),
        ("output_tokens", &mut usage.output_tokens),
        ("cache_read_input_tokens", &mut usage.cache_read_tokens),
        ("cache_creation_input_tokens", &mut usage.cache_write_tokens),
    ];
    for (wire, target) in targets {
        if let Some(value) = fields.get(wire) {
            *target = value
                .as_u64()
                .ok_or_else(|| malformed(format!("invalid {wire}")))?;
        }
    }
    Ok(())
}

fn in_band_error(event: &Map<String, Value>) -> LlmError {
    let detail = event.get("error").and_then(Value::as_object);
    let message = detail
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Messages stream error");
    match detail
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
    {
        Some("authentication_error" | "permission_error") => LlmError::unauthorized(message),
        Some("rate_limit_error") => LlmError::rate_limited(message),
        _ => LlmError::server(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::{LlmErrorCode, MessageId, ModelId, ProviderId, ToolSchema};
    use proptest::prelude::*;

    fn model() -> ModelId {
        ModelId::parse("deepseek-flash").expect("model")
    }

    fn provider() -> ProviderId {
        ProviderId::parse("deepseek").expect("provider")
    }

    fn base_request() -> LlmRequest {
        LlmRequest {
            model: model(),
            system: Some("be useful".to_string()),
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            max_tokens: 512,
            effort: ReasoningEffort::High,
            temperature: None,
            stop: Vec::new(),
        }
    }

    fn call(id: &str, arguments: &str) -> ContentBlock {
        ContentBlock::tool_call(
            dsh_core::ToolCall::new(ToolCallId::parse(id).expect("id"), "read", arguments)
                .expect("call"),
        )
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message::assistant(content, provider(), model(), TokenUsage::default())
    }

    fn json(request: &LlmRequest) -> Value {
        serde_json::to_value(serialize_request(request).expect("serialize")).expect("json")
    }

    fn messages_json(request: &LlmRequest) -> Value {
        json(request)["messages"].clone()
    }

    fn translated(values: &[Value]) -> Result<Vec<StreamEvent>, LlmError> {
        let mut translator = Translator::new();
        let mut events = Vec::new();
        for value in values {
            events.extend(translator.push(&SseEvent {
                event: None,
                data: value.to_string(),
            })?);
        }
        translator.ensure_finished()?;
        Ok(events)
    }

    fn event(pairs: &[(&str, Value)]) -> Value {
        let mut object = Map::new();
        for (key, value) in pairs {
            object.insert((*key).to_string(), value.clone());
        }
        Value::Object(object)
    }

    fn start() -> Value {
        event(&[
            ("type", Value::String("message_start".to_string())),
            (
                "message",
                serde_json::json!({"usage": {"input_tokens": 1, "output_tokens": 1}}),
            ),
        ])
    }

    fn block_start(index: u64, content_block: Value) -> Value {
        event(&[
            ("type", Value::String("content_block_start".to_string())),
            ("index", Value::from(index)),
            ("content_block", content_block),
        ])
    }

    fn block_delta(index: u64, delta: Value) -> Value {
        event(&[
            ("type", Value::String("content_block_delta".to_string())),
            ("index", Value::from(index)),
            ("delta", delta),
        ])
    }

    fn block_stop(index: u64) -> Value {
        event(&[
            ("type", Value::String("content_block_stop".to_string())),
            ("index", Value::from(index)),
        ])
    }

    fn message_delta(reason: &str) -> Value {
        event(&[
            ("type", Value::String("message_delta".to_string())),
            ("delta", serde_json::json!({"stop_reason": reason})),
        ])
    }

    fn message_stop() -> Value {
        event(&[("type", Value::String("message_stop".to_string()))])
    }

    fn text_block(text: &str) -> Value {
        serde_json::json!({"type": "text", "text": text})
    }

    fn thinking_block(text: &str) -> Value {
        serde_json::json!({"type": "thinking", "thinking": text})
    }

    fn tool_block(id: &str, name: &str) -> Value {
        serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": {}})
    }

    fn text_delta(text: &str) -> Value {
        serde_json::json!({"type": "text_delta", "text": text})
    }

    #[test]
    fn system_prompt_is_top_level_and_uses_the_latest_history_text() {
        let mut request = base_request();
        request.messages = vec![
            Message::system("old"),
            Message::user("hi"),
            Message::system("new"),
            Message::system(""),
        ];
        let value = json(&request);
        assert_eq!(value["system"], "be useful\n\nnew");
        assert_eq!(value["thinking"], serde_json::json!({"type": "enabled"}));
        assert_eq!(
            value["output_config"],
            serde_json::json!({"effort": "high"})
        );
        assert_eq!(value["stream"], true);
        assert_eq!(value["model"], "deepseek-flash");
        assert_eq!(value["max_tokens"], 512);

        request.system = None;
        request.messages = vec![Message::user("hi")];
        let value = json(&request);
        assert!(value.get("system").is_none());

        request.system = Some(String::new());
        request.messages = Vec::new();
        assert!(json(&request).get("system").is_none());
    }

    #[test]
    fn developer_notes_join_the_system_prompt() {
        let mut request = base_request();
        request.messages = vec![
            Message::system("base"),
            Message::Developer(dsh_core::DeveloperMessage {
                id: MessageId::generate(),
                text: "tools changed".to_string(),
            }),
            Message::Developer(dsh_core::DeveloperMessage {
                id: MessageId::generate(),
                text: String::new(),
            }),
        ];
        assert_eq!(
            json(&request)["system"],
            "be useful\n\nbase\n\ntools changed"
        );
    }

    #[test]
    fn user_and_assistant_blocks_map_to_wire_content() {
        let mut request = base_request();
        request.messages = vec![
            Message::user("hello"),
            assistant(vec![
                ContentBlock::text("answer"),
                ContentBlock::reasoning("because"),
                call("a", r#"{"path":"a.txt"}"#),
            ]),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "contents"),
        ];
        assert_eq!(
            messages_json(&request),
            serde_json::json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "thinking", "thinking": "because"},
                    {"type": "tool_use", "id": "a", "name": "read", "input": {"path": "a.txt"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a", "content": [{"type": "text", "text": "contents"}], "is_error": false},
                ]},
            ])
        );
    }

    #[test]
    fn adjacent_same_role_turns_merge() {
        let mut request = base_request();
        request.messages = vec![
            Message::user("one"),
            Message::user("two"),
            assistant(vec![ContentBlock::text("first")]),
            assistant(vec![ContentBlock::text("second")]),
        ];
        assert_eq!(
            messages_json(&request),
            serde_json::json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "two"},
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "first"},
                    {"type": "text", "text": "second"},
                ]},
            ])
        );
    }

    #[test]
    fn tool_results_are_user_turns_and_lead_their_turn() {
        let mut request = base_request();
        request.messages = vec![
            Message::user("hello"),
            assistant(vec![call("a", "{}"), call("b", "{}")]),
            Message::user("follow-up"),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "result"),
            Message::tool_error(ToolCallId::parse("b").expect("id"), "denied"),
        ];
        assert_eq!(
            messages_json(&request),
            serde_json::json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "a", "name": "read", "input": {}},
                    {"type": "tool_use", "id": "b", "name": "read", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a", "content": [{"type": "text", "text": "result"}], "is_error": false},
                    {"type": "tool_result", "tool_use_id": "b", "content": [{"type": "text", "text": "denied"}], "is_error": true},
                    {"type": "text", "text": "follow-up"},
                ]},
            ])
        );
    }

    #[test]
    fn empty_tool_results_keep_their_call_identity() {
        let mut request = base_request();
        request.messages = vec![
            assistant(vec![call("a", "{}")]),
            Message::tool_text(ToolCallId::parse("a").expect("id"), ""),
        ];
        assert_eq!(
            messages_json(&request)[1]["content"][0],
            serde_json::json!({"type": "tool_result", "tool_use_id": "a", "content": [], "is_error": false})
        );
    }

    #[test]
    fn malformed_historical_tool_arguments_fall_back_to_an_empty_object() {
        for raw in ["{", "[]", "null", "3", "\"text\""] {
            let mut request = base_request();
            request.messages = vec![
                assistant(vec![call("a", raw)]),
                Message::tool_text(ToolCallId::parse("a").expect("id"), "ok"),
            ];
            assert_eq!(
                messages_json(&request)[0]["content"][0]["input"],
                serde_json::json!({})
            );
        }
        let mut request = base_request();
        request.messages = vec![
            assistant(vec![call("a", r#"{"n":1}"#)]),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "ok"),
        ];
        assert_eq!(
            messages_json(&request)[0]["content"][0]["input"],
            serde_json::json!({"n": 1})
        );
    }

    #[test]
    fn empty_text_and_user_only_auxiliary_blocks_are_omitted() {
        let mut request = base_request();
        request.messages = vec![
            assistant(vec![ContentBlock::text("")]),
            Message::User(dsh_core::UserMessage {
                id: MessageId::generate(),
                content: vec![ContentBlock::reasoning("hidden"), call("x", "{}")],
            }),
            Message::user(""),
            Message::user("visible"),
        ];
        assert_eq!(
            messages_json(&request),
            serde_json::json!([{"role": "user", "content": [{"type": "text", "text": "visible"}]}])
        );
    }

    #[test]
    fn tool_pairing_accepts_answered_calls() {
        let mut request = base_request();
        request.messages = vec![
            assistant(vec![call("a", "{}"), call("b", "{}")]),
            Message::tool_text(ToolCallId::parse("b").expect("id"), "b"),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "a"),
        ];
        assert!(serialize_request(&request).is_ok());
    }

    #[test]
    fn duplicate_tool_call_ids_are_invalid() {
        let mut request = base_request();
        request.messages = vec![
            assistant(vec![call("a", "{}"), call("a", "{}")]),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "a"),
        ];
        assert_eq!(
            serialize_request(&request).expect_err("duplicate").code,
            LlmErrorCode::InvalidRequest
        );
    }

    #[test]
    fn unresolved_tool_calls_are_invalid() {
        let mut request = base_request();
        request.messages = vec![assistant(vec![call("a", "{}")])];
        assert_eq!(
            serialize_request(&request).expect_err("unresolved").code,
            LlmErrorCode::InvalidRequest
        );

        let mut request = base_request();
        request.messages = vec![assistant(vec![call("a", "{}")]), Message::user("no result")];
        assert_eq!(
            serialize_request(&request).expect_err("immediate").code,
            LlmErrorCode::InvalidRequest
        );
    }

    #[test]
    fn unexpected_tool_results_are_invalid() {
        let mut request = base_request();
        request.messages = vec![Message::tool_text(
            ToolCallId::parse("a").expect("id"),
            "result",
        )];
        assert_eq!(
            serialize_request(&request).expect_err("orphan result").code,
            LlmErrorCode::InvalidRequest
        );

        let mut request = base_request();
        request.messages = vec![
            assistant(vec![call("a", "{}")]),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "first"),
            Message::tool_text(ToolCallId::parse("a").expect("id"), "second"),
        ];
        assert_eq!(
            serialize_request(&request)
                .expect_err("duplicate result")
                .code,
            LlmErrorCode::InvalidRequest
        );
    }

    #[test]
    fn reasoning_effort_controls_thinking_and_output_config() {
        for (effort, thinking, output) in [
            (
                ReasoningEffort::Off,
                serde_json::json!({"type": "disabled"}),
                None,
            ),
            (
                ReasoningEffort::Low,
                serde_json::json!({"type": "enabled"}),
                Some(serde_json::json!({"effort": "low"})),
            ),
            (
                ReasoningEffort::High,
                serde_json::json!({"type": "enabled"}),
                Some(serde_json::json!({"effort": "high"})),
            ),
            (
                ReasoningEffort::Max,
                serde_json::json!({"type": "enabled"}),
                Some(serde_json::json!({"effort": "max"})),
            ),
        ] {
            let mut request = base_request();
            request.effort = effort;
            let value = json(&request);
            assert_eq!(value["thinking"], thinking);
            assert_eq!(value.get("output_config"), output.as_ref());
        }
    }

    #[test]
    fn tools_are_declared_with_an_input_schema() {
        let mut request = base_request();
        request.tools =
            vec![ToolSchema::new(
            "read",
            "Read a file",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )
        .expect("schema")];
        assert_eq!(
            json(&request)["tools"],
            serde_json::json!([{
                "name": "read",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
            }])
        );

        request.tools.clear();
        assert!(json(&request).get("tools").is_none());
    }

    #[test]
    fn optional_sampling_fields_are_forwarded_or_omitted() {
        let mut request = base_request();
        request.temperature = Some(0.5);
        request.stop = vec!["STOP".to_string()];
        let value = json(&request);
        assert_eq!(value["temperature"], 0.5);
        assert_eq!(value["stop_sequences"], serde_json::json!(["STOP"]));

        request.temperature = None;
        request.stop.clear();
        let value = json(&request);
        assert!(value.get("temperature").is_none());
        assert!(value.get("stop_sequences").is_none());
    }

    #[test]
    fn invalid_requests_are_rejected_before_serialization() {
        let mut request = base_request();
        request.max_tokens = 0;
        assert_eq!(
            serialize_request(&request).expect_err("max tokens").code,
            LlmErrorCode::InvalidRequest
        );

        let mut request = base_request();
        request.temperature = Some(f32::NAN);
        assert_eq!(
            serialize_request(&request).expect_err("nan").code,
            LlmErrorCode::InvalidRequest
        );
    }

    #[test]
    fn text_stream_translates_deltas_and_finish() {
        let events = translated(&[
            start(),
            block_start(0, text_block("")),
            block_delta(0, text_delta("Hel")),
            block_delta(0, text_delta("lo")),
            block_stop(0),
            message_delta("end_turn"),
            message_stop(),
        ])
        .expect("stream");
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    text: "Hel".to_string()
                },
                StreamEvent::TextDelta {
                    text: "lo".to_string()
                },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                },
                StreamEvent::Finish {
                    reason: FinishReason::Stop
                },
            ]
        );
    }

    #[test]
    fn initial_text_and_thinking_are_emitted_and_signatures_are_ignored() {
        let events = translated(&[
            start(),
            block_start(2, text_block("seed")),
            block_stop(2),
            block_start(5, thinking_block("first")),
            block_delta(
                5,
                serde_json::json!({"type": "thinking_delta", "thinking": " thought"}),
            ),
            block_delta(
                5,
                serde_json::json!({"type": "signature_delta", "signature": "sig"}),
            ),
            block_stop(5),
            block_start(6, thinking_block("")),
            block_stop(6),
            message_delta("stop_sequence"),
            message_stop(),
        ])
        .expect("stream");
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    text: "seed".to_string()
                },
                StreamEvent::ReasoningDelta {
                    text: "first".to_string()
                },
                StreamEvent::ReasoningDelta {
                    text: " thought".to_string()
                },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0
                    }
                },
                StreamEvent::Finish {
                    reason: FinishReason::Stop
                },
            ]
        );
    }

    #[test]
    fn tool_stream_translates_start_and_argument_fragments() {
        let events = translated(&[
            start(),
            block_start(4, tool_block("call-1", "read")),
            block_delta(
                4,
                serde_json::json!({"type": "input_json_delta", "partial_json": "{\"path\": "}),
            ),
            block_delta(
                4,
                serde_json::json!({"type": "input_json_delta", "partial_json": "\"a\"}"}),
            ),
            block_stop(4),
            message_delta("tool_use"),
            message_stop(),
        ])
        .expect("stream");
        let id = ToolCallId::parse("call-1").expect("id");
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart {
                    id: id.clone(),
                    name: "read".to_string()
                },
                StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    arguments: "{\"path\": ".to_string()
                },
                StreamEvent::ToolCallDelta {
                    id,
                    arguments: "\"a\"}".to_string()
                },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0
                    }
                },
                StreamEvent::Finish {
                    reason: FinishReason::ToolCalls
                },
            ]
        );
    }

    #[test]
    fn usage_is_assigned_from_start_and_delta_events() {
        let events = translated(&[
            event(&[
                ("type", Value::String("message_start".to_string())),
                (
                    "message",
                    serde_json::json!({"usage": {
                        "input_tokens": 12,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 30,
                        "cache_creation_input_tokens": 7,
                    }}),
                ),
            ]),
            event(&[
                ("type", Value::String("message_delta".to_string())),
                ("delta", serde_json::json!({"stop_reason": "max_tokens"})),
                ("usage", serde_json::json!({"output_tokens": 3})),
            ]),
            message_stop(),
        ])
        .expect("stream");
        assert_eq!(
            events,
            vec![
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 12,
                        output_tokens: 3,
                        cache_read_tokens: 30,
                        cache_write_tokens: 7,
                    },
                },
                StreamEvent::Finish {
                    reason: FinishReason::MaxTokens
                },
            ]
        );
    }

    #[test]
    fn all_stop_reasons_map_to_finish_reasons() {
        for (wire, reason) in [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("tool_use", FinishReason::ToolCalls),
            ("max_tokens", FinishReason::MaxTokens),
        ] {
            let events = translated(&[
                start(),
                block_start(0, text_block("x")),
                block_stop(0),
                message_delta(wire),
                message_stop(),
            ])
            .expect("stream");
            assert_eq!(events.last(), Some(&StreamEvent::Finish { reason }));
        }
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let events = translated(&[
            serde_json::json!({"type": "ping"}),
            start(),
            serde_json::json!({"type": "future_event", "value": 1}),
            block_start(0, text_block("x")),
            block_stop(0),
            message_delta("end_turn"),
            message_stop(),
            serde_json::json!({"type": "ping"}),
        ])
        .expect("stream");
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    text: "x".to_string()
                },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0
                    }
                },
                StreamEvent::Finish {
                    reason: FinishReason::Stop
                },
            ]
        );
    }

    #[test]
    fn events_after_message_stop_are_ignored() {
        let mut translator = Translator::new();
        for value in [
            start(),
            block_start(0, text_block("x")),
            block_stop(0),
            message_delta("end_turn"),
            message_stop(),
        ] {
            translator
                .push(&SseEvent {
                    event: None,
                    data: value.to_string(),
                })
                .expect("event");
        }
        assert!(translator.is_finished());
        assert_eq!(
            translator
                .push(&SseEvent {
                    event: None,
                    data: "not json".to_string()
                })
                .expect("ignored"),
            Vec::<StreamEvent>::new()
        );
    }

    #[test]
    fn duplicate_message_start_is_malformed() {
        let error = translated(&[start(), start()]).expect_err("duplicate start");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);
    }

    #[test]
    fn content_before_message_start_is_malformed() {
        for value in [
            block_start(0, text_block("x")),
            block_delta(0, text_delta("x")),
            block_stop(0),
            message_delta("end_turn"),
            message_stop(),
        ] {
            let error = translated(&[value]).expect_err("early event");
            assert_eq!(error.code, LlmErrorCode::MalformedResponse);
        }
    }

    #[test]
    fn duplicate_or_mixed_block_indexes_are_malformed() {
        let error = translated(&[
            start(),
            block_start(0, text_block("x")),
            block_start(0, text_block("y")),
        ])
        .expect_err("duplicate start");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);

        let error = translated(&[
            start(),
            block_start(0, text_block("x")),
            block_delta(1, text_delta("y")),
        ])
        .expect_err("unknown index");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);
    }

    #[test]
    fn deltas_and_stops_require_an_open_block() {
        let cases = [
            vec![start(), block_delta(0, text_delta("x"))],
            vec![start(), block_stop(0)],
            vec![
                start(),
                block_start(0, text_block("x")),
                block_stop(0),
                block_stop(0),
            ],
            vec![
                start(),
                block_start(0, text_block("x")),
                block_stop(0),
                block_delta(0, text_delta("x")),
            ],
            vec![
                start(),
                block_start(0, text_block("x")),
                block_delta(
                    0,
                    serde_json::json!({"type": "thinking_delta", "thinking": "x"}),
                ),
            ],
            vec![
                start(),
                block_start(0, text_block("x")),
                block_delta(0, serde_json::json!({"type": "mystery"})),
            ],
        ];
        for values in cases {
            let error = translated(&values).expect_err("malformed");
            assert_eq!(error.code, LlmErrorCode::MalformedResponse);
        }
    }

    #[test]
    fn message_stop_requires_a_settled_reason_and_closed_blocks() {
        let error = translated(&[start(), message_stop()]).expect_err("no reason");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);

        let error = translated(&[
            start(),
            block_start(0, text_block("x")),
            message_delta("end_turn"),
            message_stop(),
        ])
        .expect_err("open block");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);

        let error = translated(&[
            start(),
            message_delta("end_turn"),
            block_start(0, text_block("x")),
        ])
        .expect_err("late block");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);
    }

    #[test]
    fn empty_successful_response_is_reported() {
        let error = translated(&[start(), message_delta("end_turn"), message_stop()])
            .expect_err("empty response");
        assert_eq!(error.code, LlmErrorCode::EmptyResponse);
    }

    #[test]
    fn stream_ending_before_message_stop_is_reported() {
        let mut translator = Translator::new();
        translator
            .push(&SseEvent {
                event: None,
                data: start().to_string(),
            })
            .expect("start");
        assert_eq!(
            translator.ensure_finished().expect_err("unfinished").code,
            LlmErrorCode::StreamClosed
        );
    }

    #[test]
    fn usage_rejects_negative_and_fractional_values() {
        for usage in [
            serde_json::json!({"input_tokens": -1}),
            serde_json::json!({"output_tokens": 1.5}),
            serde_json::json!({"cache_read_input_tokens": "3"}),
            serde_json::json!({"cache_creation_input_tokens": null}),
        ] {
            let value = event(&[
                ("type", Value::String("message_start".to_string())),
                ("message", serde_json::json!({"usage": usage})),
            ]);
            let error = translated(&[value]).expect_err("bad usage");
            assert_eq!(error.code, LlmErrorCode::MalformedResponse);
        }

        let value = event(&[
            ("type", Value::String("message_start".to_string())),
            ("message", serde_json::json!({"usage": []})),
        ]);
        let error = translated(&[value]).expect_err("bad usage");
        assert_eq!(error.code, LlmErrorCode::MalformedResponse);
    }

    #[test]
    fn malformed_json_and_mismatched_event_names_are_rejected() {
        let mut translator = Translator::new();
        assert_eq!(
            translator
                .push(&SseEvent {
                    event: None,
                    data: "not-json".to_string()
                })
                .expect_err("json")
                .code,
            LlmErrorCode::MalformedResponse
        );
        assert_eq!(
            translator
                .push(&SseEvent {
                    event: None,
                    data: "[]".to_string()
                })
                .expect_err("object")
                .code,
            LlmErrorCode::MalformedResponse
        );
        assert_eq!(
            translator
                .push(&SseEvent {
                    event: None,
                    data: serde_json::json!({"value": 1}).to_string()
                })
                .expect_err("type")
                .code,
            LlmErrorCode::MalformedResponse
        );
        assert_eq!(
            translator
                .push(&SseEvent {
                    event: Some("ping".to_string()),
                    data: serde_json::json!({"type": "message_start"}).to_string(),
                })
                .expect_err("mismatch")
                .code,
            LlmErrorCode::MalformedResponse
        );
    }

    #[test]
    fn unsupported_blocks_and_missing_fields_are_rejected() {
        let cases = [
            vec![start(), block_start(0, serde_json::json!({"type": "text"}))],
            vec![
                start(),
                block_start(
                    0,
                    serde_json::json!({"type": "tool_use", "id": "", "name": "read"}),
                ),
            ],
            vec![
                start(),
                block_start(
                    0,
                    serde_json::json!({"type": "tool_use", "id": "a", "name": ""}),
                ),
            ],
            vec![
                start(),
                block_start(0, serde_json::json!({"type": "tool_use", "id": "a"})),
            ],
            vec![
                start(),
                event(&[("type", Value::String("content_block_start".to_string()))]),
            ],
            vec![
                start(),
                event(&[
                    ("type", Value::String("content_block_delta".to_string())),
                    ("index", Value::from(0)),
                    ("delta", serde_json::json!({})),
                ]),
            ],
            vec![
                start(),
                event(&[("type", Value::String("message_delta".to_string()))]),
            ],
            vec![
                start(),
                event(&[
                    ("type", Value::String("message_delta".to_string())),
                    ("delta", serde_json::json!({"stop_reason": "mystery"})),
                ]),
            ],
            vec![
                start(),
                event(&[
                    ("type", Value::String("message_delta".to_string())),
                    ("delta", serde_json::json!({"stop_reason": 3})),
                ]),
            ],
            vec![
                start(),
                event(&[
                    ("type", Value::String("content_block_start".to_string())),
                    ("index", Value::from(-1)),
                    ("content_block", text_block("x")),
                ]),
            ],
            vec![
                start(),
                event(&[
                    ("type", Value::String("content_block_start".to_string())),
                    ("index", serde_json::json!(1.5)),
                    ("content_block", text_block("x")),
                ]),
            ],
            vec![
                start(),
                event(&[
                    ("type", Value::String("content_block_delta".to_string())),
                    ("index", Value::String("0".to_string())),
                    ("delta", text_delta("x")),
                ]),
            ],
            vec![
                start(),
                event(&[("type", Value::String("content_block_stop".to_string()))]),
            ],
            vec![
                start(),
                block_start(0, thinking_block("x")),
                block_delta(0, serde_json::json!({"type": "signature_delta"})),
            ],
        ];
        for values in cases {
            let error = translated(&values).expect_err("malformed");
            assert_eq!(error.code, LlmErrorCode::MalformedResponse);
        }

        let error = translated(&[
            start(),
            block_start(0, serde_json::json!({"type": "redacted_thinking"})),
        ])
        .expect_err("unsupported");
        assert_eq!(error.code, LlmErrorCode::UnsupportedContent);
    }

    #[test]
    fn null_stop_reason_is_ignored_until_message_delta_settles() {
        let events = translated(&[
            start(),
            event(&[
                ("type", Value::String("message_delta".to_string())),
                ("delta", serde_json::json!({"stop_reason": null})),
            ]),
            block_start(0, text_block("x")),
            block_stop(0),
            message_delta("end_turn"),
            message_stop(),
        ])
        .expect("stream");
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Finish {
                reason: FinishReason::Stop
            })
        );
    }

    #[test]
    fn in_band_errors_are_classified() {
        let error = translated(&[serde_json::json!({
            "type": "error",
            "error": {"type": "authentication_error", "message": "bad key"},
        })])
        .expect_err("auth");
        assert_eq!(error.code, LlmErrorCode::Unauthorized);
        assert_eq!(error.message, "bad key");

        let error = translated(&[serde_json::json!({
            "type": "error",
            "error": {"type": "permission_error", "message": "forbidden"},
        })])
        .expect_err("permission");
        assert_eq!(error.code, LlmErrorCode::Unauthorized);

        let error = translated(&[serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "slow"},
        })])
        .expect_err("limit");
        assert_eq!(error.code, LlmErrorCode::RateLimited);

        let error = translated(&[serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error"},
        })])
        .expect_err("server");
        assert_eq!(error.code, LlmErrorCode::Server);
        assert_eq!(error.message, "Messages stream error");
    }

    #[test]
    fn image_blocks_map_to_base64_sources() {
        let image = ContentBlock::image("image/png", "AAAA");
        assert_eq!(
            user_block(&image),
            Some(WireBlock::Image {
                source: WireImageSource::base64("image/png", "AAAA"),
            })
        );
        assert_eq!(assistant_block(&image), None);
        let tool = dsh_core::ToolMessage {
            id: MessageId::generate(),
            call_id: dsh_core::ToolCallId::parse("call-1").expect("id"),
            content: vec![image],
            is_error: false,
        };
        assert_eq!(
            tool_result_block(&tool),
            WireBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: vec![WireInput::Image {
                    source: WireImageSource::base64("image/png", "AAAA"),
                }],
                is_error: Some(false),
            }
        );
    }

    proptest! {
        #[test]
        fn translation_emits_text_and_finish(text in "[ -~]{0,64}") {
            let mut translator = Translator::new();
            let mut events = Vec::new();
            let values = [
                start(),
                block_start(0, text_block("")),
                block_delta(0, text_delta(&text)),
                block_stop(0),
                message_delta("end_turn"),
                message_stop(),
            ];
            for value in values {
                events.extend(
                    translator
                        .push(&SseEvent { event: None, data: value.to_string() })
                        .expect("event"),
                );
            }
            prop_assert!(translator.is_finished());
            prop_assert_eq!(
                events.first(),
                Some(&StreamEvent::TextDelta { text })
            );
            prop_assert_eq!(
                events.last(),
                Some(&StreamEvent::Finish { reason: FinishReason::Stop })
            );
        }
    }
}
