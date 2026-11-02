#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! `SQLite` persistence for the harness session log.
//!
//! The store keeps one header row per session and an append-only event table.
//! Every mutation runs in a transaction, and the log is ordered by a
//! per-session sequence the store assigns.

mod store;

pub use store::SqliteSessionStore;
