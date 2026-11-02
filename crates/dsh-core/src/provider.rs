//! Provider routes and the table that selects the one in use.
//!
//! A route is the address of one Messages-compatible endpoint: its name, its
//! base URL, the model it serves, and the optional environment variable that
//! holds its key. The table is built once at the configuration boundary, so a
//! `Config` cannot describe an endpoint twice or select a route it lacks.

use crate::config::{DEFAULT_BASE_URL, DEFAULT_MODEL, Env, valid_base_url};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// Name of the route built from the `DEEPSEEK_*` keys.
pub const DEFAULT_PROVIDER: &str = "deepseek";

/// Longest accepted route name, in bytes; also the credential store's key limit.
pub const MAX_PROVIDER_NAME_BYTES: usize = 64;

pub(crate) const BUILTIN_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Whether `name` can key the credential store and the durable provider id.
#[must_use]
pub fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROVIDER_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// One Messages-compatible route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    /// Route identity, also the credential-store key.
    pub name: String,
    /// Messages root, without `/v1/messages`.
    pub base_url: String,
    /// Model id sent with every request on this route.
    pub model: String,
    /// Variable holding the key; `None` means the stored secret is the only source.
    pub api_key_env: Option<String>,
}

impl ProviderConfig {
    /// Resolve this route's key from the environment, if it names a variable.
    #[must_use]
    pub fn api_key(&self, env: &dyn Env) -> Option<String> {
        let var = self.api_key_env.as_deref()?;
        env.var(var).filter(|key| !key.is_empty())
    }
}

/// Every configured route, and which one is active.
///
/// Built only by [`ProviderTable::build`], so `active`, when set, indexes an
/// entry. A table with several routes and no selection leaves it unset, which
/// lets inspection commands run without naming a model route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTable {
    entries: Vec<ProviderConfig>,
    active: Option<usize>,
}

impl Default for ProviderTable {
    fn default() -> Self {
        Self {
            entries: vec![builtin_route()],
            active: Some(0),
        }
    }
}

impl ProviderTable {
    /// Merge the built-in route with the configured ones and select `active`.
    ///
    /// A sole route is selected automatically. With several configured routes
    /// and no selection, `active` stays unset so inspection commands can use
    /// the table without a model route. `model_override` has already been
    /// validated as non-blank.
    pub fn build(
        builtin: ProviderConfig,
        configured: Vec<ProviderConfig>,
        selection: Option<&str>,
        model_override: Option<String>,
    ) -> Result<Self, ProviderError> {
        let configured_count = configured.len();
        let mut entries = Vec::with_capacity(configured_count + 1);
        entries.push(builtin);
        entries.extend(configured);

        let active = match selection {
            Some(name) if name.trim().is_empty() => {
                return Err(ProviderError::BlankSelection {
                    known: join_names(&entries),
                });
            }
            Some(name) => Some(
                entries
                    .iter()
                    .position(|route| route.name == name)
                    .ok_or_else(|| ProviderError::UnknownSelection {
                        name: name.to_string(),
                        known: join_names(&entries),
                    })?,
            ),
            None if configured_count == 0 => Some(0),
            None if configured_count == 1 => Some(1),
            None => None,
        };

        if let (Some(index), Some(model)) = (active, model_override) {
            entries[index].model = model;
        }
        Ok(Self { entries, active })
    }

    #[must_use]
    pub fn active(&self) -> Option<&ProviderConfig> {
        self.active.map(|index| &self.entries[index])
    }

    /// The selected route, or the `Unselected` error naming every route.
    pub fn require_active(&self) -> Result<&ProviderConfig, ProviderError> {
        self.active().ok_or_else(|| ProviderError::Unselected {
            known: join_names(&self.entries),
        })
    }

    /// Every route, built-in first, then configuration order.
    #[must_use]
    pub fn entries(&self) -> &[ProviderConfig] {
        &self.entries
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ProviderConfig> {
        self.entries.iter().find(|route| route.name == name)
    }

    /// Route names in table order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|route| route.name.as_str())
            .collect()
    }
}

fn builtin_route() -> ProviderConfig {
    ProviderConfig {
        name: DEFAULT_PROVIDER.to_string(),
        base_url: DEFAULT_BASE_URL.to_string(),
        model: DEFAULT_MODEL.to_string(),
        api_key_env: Some(BUILTIN_API_KEY_ENV.to_string()),
    }
}

fn join_names(entries: &[ProviderConfig]) -> String {
    entries
        .iter()
        .map(|route| route.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    name: String,
    base_url: String,
    model: String,
    api_key_env: Option<String>,
}

/// Parse the `DSH_PROVIDERS` JSON array into validated routes.
pub fn parse_providers(raw: &str) -> Result<Vec<ProviderConfig>, ProviderError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| ProviderError::Encoding(error.to_string()))?;
    let entries = value
        .as_array()
        .ok_or_else(|| ProviderError::Encoding("expected a JSON array".to_string()))?;
    let mut routes: Vec<ProviderConfig> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let raw = serde_json::from_value::<RawProvider>(entry.clone()).map_err(|error| {
            ProviderError::Entry {
                index,
                reason: EntryError::Shape(error.to_string()),
            }
        })?;
        routes.push(validate_entry(index, raw, &routes)?);
    }
    Ok(routes)
}

fn validate_entry(
    index: usize,
    raw: RawProvider,
    seen: &[ProviderConfig],
) -> Result<ProviderConfig, ProviderError> {
    let reject = |reason| ProviderError::Entry { index, reason };
    if raw.name == DEFAULT_PROVIDER {
        return Err(reject(EntryError::ReservedName));
    }
    if !valid_provider_name(&raw.name) {
        return Err(reject(EntryError::InvalidName { name: raw.name }));
    }
    if seen.iter().any(|route| route.name == raw.name) {
        return Err(reject(EntryError::DuplicateName { name: raw.name }));
    }
    if !valid_base_url(&raw.base_url) {
        return Err(reject(EntryError::InvalidBaseUrl(raw.base_url)));
    }
    if raw.model.trim().is_empty() {
        return Err(reject(EntryError::BlankModel));
    }
    if raw
        .api_key_env
        .as_deref()
        .is_some_and(|var| var.trim().is_empty())
    {
        return Err(reject(EntryError::BlankApiKeyEnv));
    }
    Ok(ProviderConfig {
        name: raw.name,
        base_url: raw.base_url,
        model: raw.model,
        api_key_env: raw.api_key_env,
    })
}

/// Provider-table failures, reported at the configuration boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderError {
    /// `DSH_PROVIDERS` was not a JSON array of objects.
    #[error("DSH_PROVIDERS must be a JSON array of provider objects: {0}")]
    Encoding(String),
    /// One entry failed validation.
    #[error("DSH_PROVIDERS[{index}]: {reason}")]
    Entry { index: usize, reason: EntryError },
    /// The selected route name was empty.
    #[error("provider name must not be empty; configured providers are {known}")]
    BlankSelection {
        /// Comma-joined route names.
        known: String,
    },
    /// The selected route name is not in the table.
    #[error("unknown provider `{name}`: configured providers are {known}")]
    UnknownSelection {
        name: String,
        /// Comma-joined route names.
        known: String,
    },
    /// A table holds several routes and nothing selected one.
    #[error(
        "no provider selected; use --provider or DSH_PROVIDER; configured providers are {known}"
    )]
    Unselected {
        /// Comma-joined route names.
        known: String,
    },
}

/// Why one table entry was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EntryError {
    /// The JSON object did not match the schema.
    #[error("{0}")]
    Shape(String),
    /// The name is the built-in route's name.
    #[error("name `{DEFAULT_PROVIDER}` is reserved for the built-in route")]
    ReservedName,
    /// The name is outside the accepted charset or length.
    #[error(
        "name {name:?} is invalid: use 1 to {MAX_PROVIDER_NAME_BYTES} ASCII letters, digits, '-', '_', or '.'"
    )]
    InvalidName { name: String },
    /// A name appears twice in the array.
    #[error("name {name:?} is declared more than once")]
    DuplicateName { name: String },
    /// The base URL is not an HTTP(S) root.
    #[error("base URL must start with http:// or https:// and name a host: {0}")]
    InvalidBaseUrl(String),
    /// The model id is empty or whitespace.
    #[error("model must not be blank")]
    BlankModel,
    /// The credential variable is empty or whitespace.
    #[error("api_key_env must not be blank; omit it to use a stored credential")]
    BlankApiKeyEnv,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn entry(name: &str, base_url: &str, model: &str) -> Value {
        json!({"name": name, "base_url": base_url, "model": model})
    }

    fn entries_json(entries: &[Value]) -> String {
        Value::Array(entries.to_vec()).to_string()
    }

    fn parsed(entries: &[Value]) -> Result<Vec<ProviderConfig>, ProviderError> {
        parse_providers(&entries_json(entries))
    }

    fn builtin() -> ProviderConfig {
        ProviderConfig {
            name: DEFAULT_PROVIDER.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key_env: Some(BUILTIN_API_KEY_ENV.to_string()),
        }
    }

    fn route(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            base_url: format!("http://{name}.example"),
            model: format!("{name}-model"),
            api_key_env: None,
        }
    }

    struct MapEnv(HashMap<String, String>);

    impl Env for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn parse_providers_accepts_the_documented_shape() {
        let raw = r#"[
            {
                "name": "openrouter",
                "base_url": "https://openrouter.ai/api/anthropic",
                "model": "anthropic/claude-sonnet-4.5",
                "api_key_env": "OPENROUTER_API_KEY"
            }
        ]"#;
        assert_eq!(
            parse_providers(raw).expect("parsed"),
            vec![ProviderConfig {
                name: "openrouter".to_string(),
                base_url: "https://openrouter.ai/api/anthropic".to_string(),
                model: "anthropic/claude-sonnet-4.5".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            }]
        );
    }

    #[test]
    fn api_key_env_may_be_omitted() {
        let routes =
            parsed(&[entry("local-gw", "http://127.0.0.1:1234", "qwen3")]).expect("parsed");
        assert_eq!(routes[0].api_key_env, None);
    }

    #[test]
    fn an_empty_array_has_no_routes() {
        assert_eq!(parse_providers("[]").expect("parsed"), Vec::new());
    }

    #[test]
    fn malformed_encodings_are_rejected() {
        for raw in ["{", "[{}", "null", "\"text\"", "{}"] {
            assert!(
                matches!(parse_providers(raw), Err(ProviderError::Encoding(_))),
                "{raw} was accepted"
            );
        }
    }

    #[test]
    fn entry_shape_failures_name_their_index() {
        let error = parsed(&[
            entry("a", "https://a.example", "m"),
            json!({"name": "b", "base_url": "https://b.example", "baseURL": "x"}),
            entry("c", "https://c.example", "m"),
        ])
        .expect_err("unknown field");
        assert!(
            matches!(
                &error,
                ProviderError::Entry {
                    index: 1,
                    reason: EntryError::Shape(detail),
                } if detail.contains("unknown field")
            ),
            "{error}"
        );
        assert!(error.to_string().starts_with("DSH_PROVIDERS[1]: "));

        let error =
            parsed(&[entry("a", "https://a.example", "m"), json!({})]).expect_err("missing field");
        assert!(error.to_string().starts_with("DSH_PROVIDERS[1]: "));
        assert!(error.to_string().contains("missing field"));

        let error = parsed(&[json!(["x"])]).expect_err("not an object");
        assert!(matches!(
            error,
            ProviderError::Entry {
                index: 0,
                reason: EntryError::Shape(_),
            }
        ));
    }

    #[test]
    fn invalid_names_are_rejected() {
        for name in ["", "a b", "a/b", "ü", &"a".repeat(65)] {
            let error =
                parsed(&[entry(name, "https://example.com", "m")]).expect_err("invalid name");
            assert!(
                matches!(
                    error,
                    ProviderError::Entry {
                        index: 0,
                        reason: EntryError::InvalidName { .. },
                    }
                ),
                "{name:?} was accepted"
            );
            assert!(error.to_string().contains("1 to 64 ASCII letters"));
        }
        assert!(parsed(&[entry(&"a".repeat(64), "https://example.com", "m")]).is_ok());
    }

    #[test]
    fn invalid_urls_are_rejected() {
        for url in ["ftp://gw", "https://", "https://a b", "localhost:1234"] {
            let error = parsed(&[entry("gw", url, "m")]).expect_err("invalid base URL");
            assert!(
                matches!(
                    error,
                    ProviderError::Entry {
                        index: 0,
                        reason: EntryError::InvalidBaseUrl(_),
                    }
                ),
                "{url} was accepted"
            );
        }
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let error = parsed(&[
            entry("a", "https://a.example", "m"),
            entry("b", "https://b.example", "m"),
            entry("a", "https://a2.example", "m"),
        ])
        .expect_err("duplicate");
        assert!(
            matches!(
                &error,
                ProviderError::Entry {
                    index: 2,
                    reason: EntryError::DuplicateName { name },
                } if name == "a"
            ),
            "{error}"
        );
    }

    #[test]
    fn the_builtin_name_is_reserved() {
        let error = parsed(&[entry("deepseek", "https://gw.example", "m")]).expect_err("reserved");
        assert!(matches!(
            error,
            ProviderError::Entry {
                index: 0,
                reason: EntryError::ReservedName,
            }
        ));
        assert_eq!(
            error.to_string(),
            "DSH_PROVIDERS[0]: name `deepseek` is reserved for the built-in route"
        );
    }

    #[test]
    fn blank_fields_are_rejected() {
        assert!(matches!(
            parsed(&[entry("gw", "https://gw.example", "  ")]),
            Err(ProviderError::Entry {
                index: 0,
                reason: EntryError::BlankModel,
            })
        ));
        assert!(matches!(
            parsed(&[json!({
                "name": "gw",
                "base_url": "https://gw.example",
                "model": "m",
                "api_key_env": " ",
            })]),
            Err(ProviderError::Entry {
                index: 0,
                reason: EntryError::BlankApiKeyEnv,
            })
        ));
    }

    #[test]
    fn api_key_reads_the_named_variable() {
        let gw = ProviderConfig {
            api_key_env: Some("GW_KEY".to_string()),
            ..route("gw")
        };
        let env = MapEnv(HashMap::from([(
            "GW_KEY".to_string(),
            "secret".to_string(),
        )]));
        assert_eq!(gw.api_key(&env).as_deref(), Some("secret"));
        assert_eq!(gw.api_key(&MapEnv(HashMap::new())), None);
        let empty = MapEnv(HashMap::from([("GW_KEY".to_string(), String::new())]));
        assert_eq!(gw.api_key(&empty), None);
        assert_eq!(route("named").api_key(&env), None);
    }

    #[test]
    fn the_default_table_holds_only_the_builtin_route() {
        let table = ProviderTable::default();
        assert_eq!(table.active(), Some(&builtin()));
        assert_eq!(table.entries(), std::slice::from_ref(&builtin()));
        assert_eq!(table.names(), vec![DEFAULT_PROVIDER]);
    }

    #[test]
    fn a_sole_route_is_selected_automatically() {
        let table = ProviderTable::build(builtin(), vec![route("gw")], None, None)
            .expect("sole route selected");
        assert_eq!(table.active().expect("selected").name, "gw");
        assert_eq!(table.names(), vec![DEFAULT_PROVIDER, "gw"]);
        assert!(table.get("gw").is_some());
        assert!(table.get("missing").is_none());
    }

    #[test]
    fn an_explicit_selection_wins() {
        let table = ProviderTable::build(builtin(), vec![route("a"), route("b")], Some("b"), None)
            .expect("selected");
        let active = table.active().expect("selected");
        assert_eq!(active.name, "b");
        assert_eq!(active.base_url, "http://b.example");
    }

    #[test]
    fn an_unselected_table_with_several_routes_holds_no_active_route() {
        let table = ProviderTable::build(builtin(), vec![route("a"), route("b")], None, None)
            .expect("unselected table is valid");
        assert!(table.active().is_none());
        let error = table.require_active().expect_err("no selection");
        assert!(matches!(&error, ProviderError::Unselected { .. }));
        assert_eq!(
            error.to_string(),
            "no provider selected; use --provider or DSH_PROVIDER; configured providers are deepseek, a, b"
        );
    }

    #[test]
    fn a_model_override_without_a_selection_leaves_no_active_route() {
        let table = ProviderTable::build(
            builtin(),
            vec![route("a"), route("b")],
            None,
            Some("other-model".to_string()),
        )
        .expect("unselected table is valid");
        assert!(table.active().is_none());
        assert_eq!(table.get("a").expect("configured").model, "a-model");
    }

    #[test]
    fn an_unknown_selection_names_the_routes() {
        let error = ProviderTable::build(builtin(), vec![route("gw")], Some("gw2"), None)
            .expect_err("unknown");
        assert_eq!(
            error,
            ProviderError::UnknownSelection {
                name: "gw2".to_string(),
                known: "deepseek, gw".to_string(),
            }
        );
        assert_eq!(
            error.to_string(),
            "unknown provider `gw2`: configured providers are deepseek, gw"
        );
    }

    #[test]
    fn a_blank_selection_is_rejected() {
        let error =
            ProviderTable::build(builtin(), Vec::new(), Some("  "), None).expect_err("blank");
        assert_eq!(
            error,
            ProviderError::BlankSelection {
                known: "deepseek".to_string(),
            }
        );
    }

    #[test]
    fn a_model_override_replaces_the_active_route_model() {
        let table = ProviderTable::build(
            builtin(),
            vec![route("gw")],
            Some("gw"),
            Some("other-model".to_string()),
        )
        .expect("overridden");
        assert_eq!(table.active().expect("selected").model, "other-model");
        assert_eq!(table.get("deepseek").expect("builtin").model, DEFAULT_MODEL);
    }

    proptest! {
        #[test]
        fn accepted_names_match_the_credential_key_charset(name in "[A-Za-z0-9._-]{1,64}") {
            prop_assert!(valid_provider_name(&name));
        }

        #[test]
        fn names_with_a_character_outside_the_set_are_rejected(
            prefix in "[A-Za-z0-9._-]{0,16}",
            bad in "[^A-Za-z0-9._-]",
            suffix in "[A-Za-z0-9._-]{0,16}",
        ) {
            let name = format!("{prefix}{bad}{suffix}");
            prop_assert!(!valid_provider_name(&name));
        }

        #[test]
        fn names_over_the_byte_limit_are_rejected(extra in 1..=64usize) {
            prop_assert!(!valid_provider_name(&"a".repeat(64 + extra)));
        }
    }
}
