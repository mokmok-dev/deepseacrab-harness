//! The provider-neutral LLM seam and streaming vocabulary.

use crate::ids::{ModelId, ToolCallId};
use crate::message::{Message, TokenUsage};
use crate::tool::ToolSchema;
use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::pin::Pin;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// How much reasoning the route should spend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Disable thinking.
    Off,
    /// Spend little reasoning.
    Low,
    /// Spend normal reasoning.
    #[default]
    High,
    /// Spend the maximum allowed reasoning.
    Max,
}

impl ReasoningEffort {
    /// Parse the spelling accepted by configuration.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// The stable spelling used by configuration and wire payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Whether the route should send a thinking budget.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// One model request assembled by the loop.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Route model id.
    pub model: ModelId,
    /// Rendered system prompt.
    pub system: Option<String>,
    /// Derived conversation history.
    pub messages: Vec<Message>,
    /// Tool declarations shown to the model.
    pub tools: Vec<ToolSchema>,
    /// Per-request output cap.
    pub max_tokens: u32,
    /// Reasoning effort.
    pub effort: ReasoningEffort,
    /// Optional sampling temperature.
    pub temperature: Option<f32>,
    /// Stop sequences.
    pub stop: Vec<String>,
}

impl LlmRequest {
    /// Validate caller-controlled request fields before any network work.
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.max_tokens == 0 {
            return Err(LlmError::invalid_request(
                "max_tokens must be greater than zero",
            ));
        }
        if let Some(temperature) = self.temperature {
            if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
                return Err(LlmError::invalid_request(
                    "temperature must be between 0 and 2",
                ));
            }
        }
        if self.stop.iter().any(String::is_empty) {
            return Err(LlmError::invalid_request(
                "stop sequences must not be empty",
            ));
        }
        if self.tools.iter().any(|tool| tool.name.is_empty()) {
            return Err(LlmError::invalid_request("tool names must not be empty"));
        }
        Ok(())
    }
}

/// A decoded streaming frame from one model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Visible answer text.
    TextDelta {
        /// Newly arrived text.
        text: String,
    },
    /// Reasoning text.
    ReasoningDelta {
        /// Newly arrived reasoning.
        text: String,
    },
    /// A tool call block opened.
    ToolCallStart {
        /// Provider-issued call identity.
        id: ToolCallId,
        /// Tool name.
        name: String,
    },
    /// An incremental tool argument fragment.
    ToolCallDelta {
        /// Call identity.
        id: ToolCallId,
        /// Raw JSON fragment.
        arguments: String,
    },
    /// Cumulative token usage for the call.
    Usage {
        /// Reported usage.
        usage: TokenUsage,
    },
    /// The provider settled the call.
    Finish {
        /// Why it stopped.
        reason: FinishReason,
    },
}

/// Why a model call stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished its answer.
    Stop,
    /// The model requested tools.
    ToolCalls,
    /// The output cap was reached.
    MaxTokens,
}

impl FinishReason {
    /// Decode the provider stop reason.
    #[must_use]
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "end_turn" | "stop_sequence" | "stop" => Some(Self::Stop),
            "tool_use" | "tool_calls" => Some(Self::ToolCalls),
            "max_tokens" | "length" => Some(Self::MaxTokens),
            _ => None,
        }
    }
}

/// Machine-routing codes for provider and transport failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorCode {
    /// No credential resolved for the route.
    MissingCredential,
    /// The provider rejected the credential.
    Unauthorized,
    /// The provider rate-limited the caller.
    RateLimited,
    /// The provider returned a server failure.
    Server,
    /// The transport failed before a response settled.
    Transport,
    /// The response did not match the protocol.
    MalformedResponse,
    /// The stream ended before a terminal event.
    StreamClosed,
    /// The request was invalid before dispatch.
    InvalidRequest,
    /// The request contained content the route cannot represent.
    UnsupportedContent,
    /// The provider settled with no content.
    EmptyResponse,
}

impl LlmErrorCode {
    /// Stable spelling used in diagnostics and JSON events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingCredential => "MISSING_CREDENTIAL",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::RateLimited => "RATE_LIMITED",
            Self::Server => "SERVER_ERROR",
            Self::Transport => "TRANSPORT_ERROR",
            Self::MalformedResponse => "MALFORMED_RESPONSE",
            Self::StreamClosed => "STREAM_CLOSED",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::UnsupportedContent => "UNSUPPORTED_CONTENT",
            Self::EmptyResponse => "EMPTY_RESPONSE",
        }
    }
}

/// One provider or transport failure with retry guidance.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}: {}", .code.as_str(), .message)]
pub struct LlmError {
    /// Stable machine code.
    pub code: LlmErrorCode,
    /// Human-readable detail.
    pub message: String,
    /// Whether a later attempt may succeed.
    pub retryable: bool,
    /// HTTP status when one was available.
    pub status: Option<u16>,
}

impl LlmError {
    /// Build an error with explicit retry guidance.
    #[must_use]
    pub fn new(code: LlmErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            status: None,
        }
    }

    /// Attach an HTTP status.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// The configuration carries no credential.
    #[must_use]
    pub fn missing_credential(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::MissingCredential, message, false)
    }

    /// The provider refused the credential.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::Unauthorized, message, false)
    }

    /// The provider asked the caller to slow down.
    #[must_use]
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::RateLimited, message, true)
    }

    /// The provider returned a server failure.
    #[must_use]
    pub fn server(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::Server, message, true)
    }

    /// The transport failed.
    #[must_use]
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::Transport, message, true)
    }

    /// The response violated the protocol.
    #[must_use]
    pub fn malformed(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::MalformedResponse, message, false)
    }

    /// The stream ended before a terminal event.
    #[must_use]
    pub fn stream_closed(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::StreamClosed, message, true)
    }

    /// The request was invalid before dispatch.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::InvalidRequest, message, false)
    }

    /// The route cannot represent the content.
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::UnsupportedContent, message, false)
    }

    /// The provider settled without content.
    #[must_use]
    pub fn empty_response(message: impl Into<String>) -> Self {
        Self::new(LlmErrorCode::EmptyResponse, message, true)
    }

    /// Whether a caller may retry this failure.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// Classify an HTTP status and response detail.
    #[must_use]
    pub fn from_status(status: u16, detail: impl Into<String>) -> Self {
        let message = detail.into();
        match status {
            401 | 403 => Self::unauthorized(message).with_status(status),
            429 => Self::rate_limited(message).with_status(status),
            400..=499 => Self::invalid_request(message).with_status(status),
            500..=599 => Self::server(message).with_status(status),
            _ => Self::malformed(message).with_status(status),
        }
    }
}

impl fmt::Display for LlmErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A boxed stream of decoded model events.
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

/// A model route that can stream one call.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable route name used in diagnostics.
    fn name(&self) -> &'static str;

    /// Start one stream. The returned stream owns the request and observes
    /// `cancel` between frames.
    async fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmStream, LlmError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ModelId;
    use proptest::prelude::*;

    fn request() -> LlmRequest {
        LlmRequest {
            model: ModelId::parse("deepseek-flash").expect("model"),
            system: Some("be useful".to_string()),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 1024,
            effort: ReasoningEffort::High,
            temperature: None,
            stop: Vec::new(),
        }
    }

    proptest! {
        #[test]
        fn effort_spellings_round_trip(raw in prop_oneof![
            Just("off".to_string()),
            Just("low".to_string()),
            Just("high".to_string()),
            Just("max".to_string()),
        ]) {
            let effort = ReasoningEffort::parse(&raw).expect("known effort");
            prop_assert_eq!(effort.as_str(), raw.as_str());
        }

        #[test]
        fn retryable_codes_stay_retryable(code in prop_oneof![
            Just(LlmErrorCode::RateLimited),
            Just(LlmErrorCode::Server),
            Just(LlmErrorCode::Transport),
            Just(LlmErrorCode::StreamClosed),
            Just(LlmErrorCode::EmptyResponse),
        ]) {
            let error = LlmError::new(code, "detail", true);
            prop_assert!(error.is_retryable());
        }

        #[test]
        fn terminal_codes_stay_terminal(code in prop_oneof![
            Just(LlmErrorCode::MissingCredential),
            Just(LlmErrorCode::Unauthorized),
            Just(LlmErrorCode::MalformedResponse),
            Just(LlmErrorCode::InvalidRequest),
            Just(LlmErrorCode::UnsupportedContent),
        ]) {
            let error = LlmError::new(code, "detail", false);
            prop_assert!(!error.is_retryable());
        }
    }

    #[test]
    fn reasoning_effort_off_disables_thinking() {
        assert!(!ReasoningEffort::Off.is_enabled());
        assert!(ReasoningEffort::Low.is_enabled());
        assert!(ReasoningEffort::High.is_enabled());
        assert!(ReasoningEffort::Max.is_enabled());
        assert_eq!(ReasoningEffort::default(), ReasoningEffort::High);
    }

    #[test]
    fn status_classification_matches_http_semantics() {
        assert_eq!(
            LlmError::from_status(401, "x").code,
            LlmErrorCode::Unauthorized
        );
        assert_eq!(
            LlmError::from_status(403, "x").code,
            LlmErrorCode::Unauthorized
        );
        assert_eq!(
            LlmError::from_status(429, "x").code,
            LlmErrorCode::RateLimited
        );
        assert_eq!(
            LlmError::from_status(422, "x").code,
            LlmErrorCode::InvalidRequest
        );
        assert_eq!(LlmError::from_status(500, "x").code, LlmErrorCode::Server);
        assert_eq!(
            LlmError::from_status(302, "x").code,
            LlmErrorCode::MalformedResponse
        );
        assert_eq!(LlmError::from_status(500, "x").status, Some(500));
    }

    #[test]
    fn finish_reasons_cover_both_wire_dialects() {
        for raw in ["end_turn", "stop_sequence", "stop"] {
            assert_eq!(FinishReason::from_wire(raw), Some(FinishReason::Stop));
        }
        for raw in ["tool_use", "tool_calls"] {
            assert_eq!(FinishReason::from_wire(raw), Some(FinishReason::ToolCalls));
        }
        for raw in ["max_tokens", "length"] {
            assert_eq!(FinishReason::from_wire(raw), Some(FinishReason::MaxTokens));
        }
        assert_eq!(FinishReason::from_wire("refusal"), None);
    }

    #[test]
    fn request_validation_rejects_bad_fields() {
        let mut invalid = request();
        invalid.max_tokens = 0;
        assert_eq!(
            invalid.validate(),
            Err(LlmError::invalid_request(
                "max_tokens must be greater than zero"
            ))
        );

        let mut invalid = request();
        invalid.temperature = Some(f32::NAN);
        assert!(invalid.validate().is_err());
        invalid.temperature = Some(2.5);
        assert!(invalid.validate().is_err());
        invalid.temperature = Some(2.0);
        assert!(invalid.validate().is_ok());

        let mut invalid = request();
        invalid.stop = vec![String::new()];
        assert!(invalid.validate().is_err());

        let mut invalid = request();
        invalid.tools = vec![ToolSchema {
            name: String::new(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }];
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn error_display_uses_the_machine_code() {
        let error = LlmError::missing_credential("no key");
        assert_eq!(error.to_string(), "MISSING_CREDENTIAL: no key");
        assert_eq!(
            LlmErrorCode::MalformedResponse.as_str(),
            "MALFORMED_RESPONSE"
        );
    }

    #[test]
    fn retry_guidance_is_observable() {
        assert!(LlmError::rate_limited("slow down").is_retryable());
        assert!(LlmError::server("boom").is_retryable());
        assert!(LlmError::transport("reset").is_retryable());
        assert!(!LlmError::missing_credential("no key").is_retryable());
        assert!(!LlmError::unauthorized("denied").is_retryable());
        assert!(!LlmError::malformed("bad").is_retryable());
        assert!(!LlmError::invalid_request("bad").is_retryable());
    }

    #[test]
    fn error_codes_display_their_wire_spelling() {
        assert_eq!(
            LlmErrorCode::MalformedResponse.to_string(),
            "MALFORMED_RESPONSE"
        );
        assert_eq!(LlmErrorCode::RateLimited.to_string(), "RATE_LIMITED");
    }
}
