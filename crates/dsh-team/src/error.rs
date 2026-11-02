//! Failures surfaced by the team file and its tools.

use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Team persistence or lookup failures.
#[derive(Debug, Error)]
pub enum TeamError {
    /// The team file could not be read.
    #[error("cannot read team file {path}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The team file or its temporary sibling could not be written.
    #[error("cannot write team file {path}: {source}")]
    Write {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file does not hold a team state object.
    #[error("team file encoding failure: {0}")]
    Encoding(String),
    /// A member or task value is not usable.
    #[error("invalid team value: {0}")]
    Invalid(String),
    /// No teammate has this id.
    #[error("unknown teammate {0}")]
    UnknownMember(String),
    /// No task has this id.
    #[error("unknown team task {0}")]
    UnknownTask(String),
    /// The teammate already has a turn in flight.
    #[error("teammate {0} is still running")]
    Running(String),
}
