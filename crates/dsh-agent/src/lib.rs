#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::ref_option)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! The turn and step driver.
//!
//! One turn admits a user message, then runs model steps until the model stops
//! requesting tools or a policy budget ends it. Every accepted fact is appended
//! to the session log before the next step derives its request from it.

pub mod assembler;
pub mod runner;

pub use assembler::AssistantAssembler;
pub use runner::{
    AgentConfig, AgentError, AgentLoop, EventSink, RunEvent, RunOutcome, plan_batches,
};
