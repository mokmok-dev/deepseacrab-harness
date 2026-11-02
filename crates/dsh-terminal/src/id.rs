//! The terminal id rule shared by the read, write, and close tools.

/// Whether `id` may name a terminal.
///
/// Ids are process-local map keys, so the validator only keeps them a single
/// readable token rather than acting as a path guard.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_and_path_shaped_ids_are_rejected() {
        for id in ["", "..", ".", "a/b", "a\\b", "a.b", "a b", "a\nb", "a\0b"] {
            assert!(!valid_id(id), "{id:?}");
        }
    }

    #[test]
    fn generated_ids_are_accepted() {
        assert!(valid_id(&uuid::Uuid::new_v4().simple().to_string()));
        assert!(valid_id("terminal-1_a"));
    }

    proptest! {
        #[test]
        fn tokens_are_accepted(candidate in "[A-Za-z0-9_-]{1,64}") {
            prop_assert!(valid_id(&candidate));
        }

        #[test]
        fn path_syntax_never_survives_validation(
            prefix in "[A-Za-z0-9_-]{0,8}",
            suffix in "[A-Za-z0-9_-]{0,8}",
            injected in prop_oneof![
                Just("/"), Just("\\"), Just(".."), Just("."), Just("\n"), Just(" "), Just("\0"),
            ],
        ) {
            let candidate = format!("{prefix}{injected}{suffix}");
            prop_assert!(!valid_id(&candidate));
        }

        #[test]
        fn arbitrary_strings_are_valid_only_when_they_are_tokens(candidate in ".{0,32}") {
            let is_token = candidate
                .chars()
                .all(|character| {
                    character.is_ascii_alphanumeric() || character == '-' || character == '_'
                });
            prop_assert_eq!(valid_id(&candidate), !candidate.is_empty() && is_token);
        }
    }
}
