//! Opaque identifiers and event positions.
//!
//! Each identifier is a distinct newtype so callers cannot pass a session id
//! where a message id belongs. Creation validates at the boundary; downstream
//! code trusts the type.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

/// Rejection of an opaque identifier supplied by a caller or a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IdError {
    /// The identifier was empty.
    #[error("identifier must not be empty")]
    Empty,
    /// The identifier exceeded the storage limit.
    #[error("identifier must be at most {MAX_ID_LEN} bytes")]
    TooLong,
    /// The identifier contained a control character.
    #[error("identifier must not contain control characters")]
    Control,
}

/// Maximum accepted identifier length in bytes.
pub const MAX_ID_LEN: usize = 256;

fn validate(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(IdError::Control);
    }
    Ok(())
}

macro_rules! string_id {
    ($name:ident, $prefix:literal) => {
        #[doc = concat!("Opaque ", stringify!($name), " generated with the `", $prefix, "` prefix.")]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validate an opaque string at a boundary.
            pub fn parse(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate(&value)?;
                Ok(Self(value))
            }

            /// Mint a fresh identifier.
            #[must_use]
            pub fn generate() -> Self {
                Self(format!("{}{}", $prefix, Uuid::new_v4()))
            }

            /// Borrow the stored string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

string_id!(SessionId, "session-");
string_id!(MessageId, "msg-");
string_id!(TurnId, "turn-");
string_id!(StepId, "step-");
string_id!(ToolCallId, "call-");
string_id!(ProviderId, "provider-");
string_id!(ModelId, "model-");

/// A non-negative position in a session event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSeq(u64);

impl EventSeq {
    /// The first event position in every session.
    pub const FIRST: Self = Self(0);

    /// Admit a raw position.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Read the raw position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next position, or `None` at the numeric ceiling.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn valid_id() -> impl Strategy<Value = String> {
        "[A-Za-z0-9_-]{1,64}".prop_map(String::from)
    }

    proptest! {
        #[test]
        fn valid_identifiers_round_trip(raw in valid_id()) {
            let id = SessionId::parse(raw.clone()).expect("valid id");
            prop_assert_eq!(id.as_str(), raw.as_str());
            prop_assert_eq!(id.to_string(), raw);
        }

        #[test]
        fn empty_identifiers_are_rejected(_unit in Just(())) {
            prop_assert_eq!(SessionId::parse(""), Err(IdError::Empty));
            prop_assert_eq!(MessageId::parse(""), Err(IdError::Empty));
            prop_assert_eq!(ToolCallId::parse(""), Err(IdError::Empty));
        }

        #[test]
        fn control_characters_are_rejected(prefix in valid_id(), bad in 0u8..=31, suffix in valid_id()) {
            let raw = format!("{prefix}{}{suffix}", char::from(bad));
            prop_assert_eq!(SessionId::parse(raw), Err(IdError::Control));
        }

        #[test]
        fn overlength_identifiers_are_rejected(extra in 1usize..64) {
            let raw = "a".repeat(MAX_ID_LEN + extra);
            prop_assert_eq!(SessionId::parse(raw), Err(IdError::TooLong));
        }

        #[test]
        fn generated_ids_keep_their_prefix_and_validate(_unit in Just(())) {
            let id = SessionId::generate();
            prop_assert!(id.as_str().starts_with("session-"));
            prop_assert!(SessionId::parse(id.as_str()).is_ok());
        }

        #[test]
        fn event_sequence_advances_by_one(start in 0u64..u64::MAX) {
            let current = EventSeq::new(start);
            let next = current.checked_next().expect("room to advance");
            prop_assert_eq!(next.get(), start + 1);
        }
    }

    #[test]
    fn sequence_ceiling_has_no_successor() {
        assert_eq!(EventSeq::new(u64::MAX).checked_next(), None);
    }

    #[test]
    fn maximum_length_identifiers_are_accepted() {
        let raw = "a".repeat(MAX_ID_LEN);
        assert!(SessionId::parse(raw.clone()).is_ok());
        assert_eq!(
            SessionId::parse("a".repeat(MAX_ID_LEN + 1)),
            Err(IdError::TooLong)
        );
        assert_eq!(raw.len(), MAX_ID_LEN);
    }

    #[test]
    fn event_sequences_display_their_value() {
        assert_eq!(EventSeq::new(0).to_string(), "0");
        assert_eq!(EventSeq::new(42).to_string(), "42");
    }

    #[test]
    fn generated_ids_are_distinct() {
        let first = MessageId::generate();
        let second = MessageId::generate();
        assert_ne!(first, second);
    }
}
