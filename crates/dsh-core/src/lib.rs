#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Domain vocabulary and capability seams shared by every harness crate.
//!
//! The crate mirrors the upstream split between durable session facts, the
//! provider-neutral message model, and swappable capabilities. Adapters live in
//! sibling crates; this crate owns the types they exchange and the plugin host
//! that composes them.

pub mod config;
pub mod event;
pub mod ids;
pub mod llm;
pub mod message;
pub mod permission;
pub mod plugin;
pub mod provider;
pub mod sandbox;
pub mod session;
pub mod store;
pub mod tool;

pub use config::{Config, ConfigError, ConfigOverrides, Env, SystemEnv, default_home};
pub use event::{
    EventRecord, GoalPhase, SessionEvent, TodoItem, TodoStatus, TurnOutcome, derive_messages,
};
pub use ids::{
    EventSeq, IdError, MAX_ID_LEN, MessageId, ModelId, ProviderId, SessionId, StepId, ToolCallId,
    TurnId,
};
pub use llm::{
    FinishReason, LlmError, LlmErrorCode, LlmProvider, LlmRequest, LlmStream, ReasoningEffort,
    StreamEvent,
};
pub use message::{
    AssistantMessage, ContentBlock, DeveloperMessage, Message, MessageError, SystemMessage,
    TokenUsage, ToolCall, ToolMessage, UserMessage,
};
pub use permission::{PathError, ensure_within, normalize_lexical, resolve_path};
pub use plugin::{
    Clock, ComposeError, Effect, FixedClock, Plugin, PluginContext, PluginError, Registry, Runtime,
    SystemClock, compose,
};
pub use provider::{
    AuthStyle, DEFAULT_PROVIDER, EntryError, MAX_PROVIDER_NAME_BYTES, ProviderConfig,
    ProviderError, ProviderTable, parse_providers, valid_provider_name,
};
pub use sandbox::{NoSandbox, Sandbox, SandboxCommand, SandboxMode};
pub use session::{SESSION_FORMAT_VERSION, SessionHeader, SessionSearchHit, SessionSummary};
pub use store::{SessionStore, StoreError};
pub use tool::{
    PermissionMode, Tool, ToolContext, ToolOutcome, ToolRegistry, ToolRegistryError, ToolSchema,
    ToolSession,
};
