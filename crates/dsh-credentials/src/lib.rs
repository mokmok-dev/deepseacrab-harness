#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Local provider credentials for the harness.
//!
//! Secrets live in one JSON object keyed by provider name. The CLI's `auth`
//! commands own the file, and the `DeepSeek` provider reads it as the fallback
//! for a missing API key environment variable.

mod store;

pub use store::{CredentialError, CredentialStore};

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    const FILE_NAME: &str = "credentials.json";

    fn store(directory: &tempfile::TempDir) -> CredentialStore {
        CredentialStore::new(directory.path().join(FILE_NAME))
    }

    fn invalid_provider_error<T: std::fmt::Debug>(result: &Result<T, CredentialError>) -> bool {
        matches!(result, Err(CredentialError::InvalidProvider { .. }))
    }

    #[test]
    fn missing_file_loads_as_empty_map() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        assert!(store.load().unwrap().is_empty());
        assert_eq!(store.providers().unwrap(), Vec::<String>::new());
        assert_eq!(store.get("deepseek").unwrap(), None);
    }

    #[test]
    fn a_failed_read_that_is_not_a_missing_file_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        fs::create_dir(&path).unwrap();
        let store = CredentialStore::new(&path);
        assert!(matches!(store.load(), Err(CredentialError::Read { .. })));
    }

    #[test]
    fn path_is_the_configured_file() {
        let directory = tempfile::tempdir().unwrap();
        let path: PathBuf = directory.path().join(FILE_NAME);
        assert_eq!(CredentialStore::new(&path).path(), path.as_path());
    }

    #[test]
    fn set_get_providers_and_remove_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        store.set("deepseek", "sk-first").unwrap();
        store.set("openrouter", "sk-second").unwrap();
        assert_eq!(store.get("deepseek").unwrap().as_deref(), Some("sk-first"));
        assert_eq!(
            store.providers().unwrap(),
            vec!["deepseek".to_string(), "openrouter".to_string()]
        );
        assert!(store.remove("deepseek").unwrap());
        assert_eq!(store.get("deepseek").unwrap(), None);
        assert_eq!(store.providers().unwrap(), vec!["openrouter".to_string()]);
        let reloaded = CredentialStore::new(store.path());
        assert_eq!(
            reloaded.load().unwrap(),
            BTreeMap::from([("openrouter".to_string(), "sk-second".to_string())])
        );
    }

    #[test]
    fn set_replaces_an_existing_secret() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        store.set("deepseek", "sk-first").unwrap();
        store.set("deepseek", "sk-second").unwrap();
        assert_eq!(
            store.load().unwrap(),
            BTreeMap::from([("deepseek".to_string(), "sk-second".to_string())])
        );
    }

    #[test]
    fn invalid_provider_names_are_rejected_by_every_entry_point() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        for name in ["", " ", "deep seek", "deep/seek", "deep:seek", "deepé"] {
            assert!(
                invalid_provider_error(&store.set(name, "sk-secret")),
                "{name:?} was accepted by set"
            );
            assert!(
                invalid_provider_error(&store.get(name)),
                "{name:?} was accepted by get"
            );
            assert!(
                invalid_provider_error(&store.remove(name)),
                "{name:?} was accepted by remove"
            );
        }
        assert!(!store.path().exists());
    }

    #[test]
    fn provider_name_length_boundary_is_sixty_four_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        let longest = "a".repeat(64);
        store.set(&longest, "sk-secret").unwrap();
        assert_eq!(store.get(&longest).unwrap().as_deref(), Some("sk-secret"));
        assert!(invalid_provider_error(
            &store.set(&"a".repeat(65), "sk-secret")
        ));
    }

    #[test]
    fn provider_name_accepts_letters_digits_and_the_three_punctuation_marks() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        store.set("DeepSeek-v4.1_flash", "sk-mixed").unwrap();
        assert_eq!(
            store.get("DeepSeek-v4.1_flash").unwrap().as_deref(),
            Some("sk-mixed")
        );
    }

    #[test]
    fn empty_secret_is_rejected_and_leaves_no_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        assert!(matches!(
            store.set("deepseek", ""),
            Err(CredentialError::EmptySecret { .. })
        ));
        assert!(!store.path().exists());
    }

    #[test]
    fn malformed_json_is_an_encoding_error_and_is_not_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let original = b"{\"deepseek\": ";
        fs::write(&path, original).unwrap();
        let store = CredentialStore::new(&path);
        assert!(matches!(store.load(), Err(CredentialError::Encoding(_))));
        assert!(matches!(
            store.get("deepseek"),
            Err(CredentialError::Encoding(_))
        ));
        assert!(matches!(
            store.set("deepseek", "sk-new"),
            Err(CredentialError::Encoding(_))
        ));
        assert!(matches!(
            store.remove("deepseek"),
            Err(CredentialError::Encoding(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!directory.path().join("credentials.json.tmp").exists());
    }

    #[test]
    fn non_object_json_is_an_encoding_error() {
        for content in ["[]", "\"text\"", "null", "3", "{\"deepseek\": 7}"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(FILE_NAME);
            fs::write(&path, content).unwrap();
            let store = CredentialStore::new(&path);
            assert!(
                matches!(store.load(), Err(CredentialError::Encoding(_))),
                "{content} was accepted"
            );
            assert_eq!(fs::read(&path).unwrap(), content.as_bytes());
        }
    }

    #[test]
    fn save_leaves_no_temporary_sibling() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        store.set("deepseek", "sk-secret").unwrap();
        let mut names = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from(FILE_NAME)]);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path().join("nested/deeper").join(FILE_NAME));
        store.set("deepseek", "sk-nested").unwrap();
        assert_eq!(store.get("deepseek").unwrap().as_deref(), Some("sk-nested"));
    }

    #[test]
    fn remove_of_an_absent_provider_reports_false_and_keeps_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let original = b"{\"deepseek\":\"sk-kept\"}";
        fs::write(&path, original).unwrap();
        let store = CredentialStore::new(&path);
        assert!(!store.remove("openrouter").unwrap());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!directory.path().join("credentials.json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory);
        store.set("deepseek", "sk-secret").unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn valid_byte() -> impl Strategy<Value = u8> {
        prop_oneof![
            b'a'..=b'z',
            b'A'..=b'Z',
            b'0'..=b'9',
            Just(b'-'),
            Just(b'_'),
            Just(b'.'),
        ]
    }

    fn provider_name() -> impl Strategy<Value = String> {
        prop::collection::vec(valid_byte(), 1..=64)
            .prop_map(|bytes| String::from_utf8(bytes).expect("generated ASCII"))
    }

    fn secret() -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 1..=32).prop_map(String::from_iter)
    }

    proptest! {
        #[test]
        fn valid_names_round_trip_through_the_file(
            entries in prop::collection::btree_map(provider_name(), secret(), 1..6),
        ) {
            let directory = tempfile::tempdir().unwrap();
            let store = CredentialStore::new(directory.path().join(FILE_NAME));
            for (provider, value) in &entries {
                store.set(provider, value).unwrap();
                let stored = store.get(provider).unwrap();
                prop_assert_eq!(stored.as_deref(), Some(value.as_str()));
            }
            prop_assert_eq!(
                store.providers().unwrap(),
                entries.keys().cloned().collect::<Vec<_>>()
            );
            prop_assert!(store.load().unwrap() == entries);
            for provider in entries.keys() {
                prop_assert!(store.remove(provider).unwrap());
                prop_assert_eq!(store.get(provider).unwrap(), None);
            }
            prop_assert!(store.load().unwrap().is_empty());
        }

        #[test]
        fn names_with_a_character_outside_the_accepted_set_are_rejected(
            prefix in prop::collection::vec(valid_byte(), 0..=16),
            bad in "[^A-Za-z0-9._-]",
            suffix in prop::collection::vec(valid_byte(), 0..=16),
        ) {
            let name = format!(
                "{}{bad}{}",
                String::from_utf8(prefix).expect("generated ASCII"),
                String::from_utf8(suffix).expect("generated ASCII"),
            );
            let directory = tempfile::tempdir().unwrap();
            let store = CredentialStore::new(directory.path().join(FILE_NAME));
            prop_assert!(invalid_provider_error(&store.set(&name, "sk-secret")));
            prop_assert!(invalid_provider_error(&store.get(&name)));
            prop_assert!(invalid_provider_error(&store.remove(&name)));
            prop_assert!(!store.path().exists());
        }

        #[test]
        fn names_longer_than_sixty_four_bytes_are_rejected(extra in 1..=64usize) {
            let name = "a".repeat(64 + extra);
            let directory = tempfile::tempdir().unwrap();
            let store = CredentialStore::new(directory.path().join(FILE_NAME));
            prop_assert!(invalid_provider_error(&store.set(&name, "sk-secret")));
        }
    }
}
