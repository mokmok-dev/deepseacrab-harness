#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Durable agent teams for the harness.
//!
//! A team is the member list plus the shared task list in one JSON file under
//! the runtime's state directory. Each member owns a child session driven by
//! [`dsh_agent::AgentLoop`] on a background Tokio task. The live task handle is
//! process-local; only the member status that task settles is persisted.

mod error;
mod file;
mod model;
mod runtime;
mod tools;

#[cfg(test)]
mod test_support;

pub use error::TeamError;
pub use model::{MemberStatus, TaskStatus, TeamMember, TeamTask};
pub use runtime::TeamRuntime;
