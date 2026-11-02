//! Deployment configuration parsed once at the process boundary.

use crate::llm::ReasoningEffort;
use crate::tool::PermissionMode;
use std::path::PathBuf;
use thiserror::Error;

/// Default route model id for the `DeepSeek` Messages API.
pub const DEFAULT_MODEL: &str = "deepseek-flash";
/// Default `DeepSeek` Messages root.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/anthropic";
/// Default output cap per request.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Default step budget for one turn.
pub const DEFAULT_MAX_STEPS: u32 = 24;
/// Default parallel tool-call fan-out.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;

/// Default instruction block sent ahead of every conversation.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a coding agent running inside a terminal harness. Work in small, verifiable steps.
Inspect the workspace before changing it, use the provided tools, and report what you changed and what you verified.
Prefer reading and searching to guessing. Keep the final answer short and factual.";

/// Environment access, injectable for tests.
pub trait Env: Send + Sync {
    /// Read one variable, returning `None` when it is absent.
    fn var(&self, key: &str) -> Option<String>;
}

/// Process environment access.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Validated deployment configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Route model id.
    pub model: String,
    /// Messages-compatible base URL.
    pub base_url: String,
    /// Environment variable that holds the API key.
    pub api_key_env: String,
    /// Per-request output cap.
    pub max_tokens: u32,
    /// Reasoning effort.
    pub effort: ReasoningEffort,
    /// Step budget for one turn.
    pub max_steps: u32,
    /// Maximum parallel tool calls.
    pub max_parallel_tools: usize,
    /// Retryable provider failures retried before the turn fails.
    pub max_retries: u32,
    /// Base delay for exponential retry backoff.
    pub retry_base_ms: u64,
    /// Estimated history characters that trigger compaction.
    pub max_context_chars: usize,
    /// Effective permission mode.
    pub permission: PermissionMode,
    /// Harness home directory for sessions and configuration.
    pub home: PathBuf,
    /// System instruction block.
    pub system_prompt: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            effort: ReasoningEffort::High,
            max_steps: DEFAULT_MAX_STEPS,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            max_retries: 2,
            retry_base_ms: 500,
            max_context_chars: 200_000,
            permission: PermissionMode::WorkspaceWrite,
            home: default_home(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }
}

impl Config {
    /// Parse configuration from a key lookup, so callers can test without mutating process state.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let model = lookup("DEEPSEEK_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
        if model.trim().is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "DEEPSEEK_MODEL",
                value: model,
            });
        }

        let base_url = lookup("DEEPSEEK_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        if !valid_base_url(&base_url) {
            return Err(ConfigError::InvalidBaseUrl(base_url));
        }

        let api_key_env =
            lookup("DSH_API_KEY_ENV").unwrap_or_else(|| "DEEPSEEK_API_KEY".to_string());
        if api_key_env.trim().is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "DSH_API_KEY_ENV",
                value: api_key_env,
            });
        }

        let max_tokens = parse_positive_u32(&lookup, "DSH_MAX_TOKENS", DEFAULT_MAX_TOKENS)?;
        let max_steps = parse_positive_u32(&lookup, "DSH_MAX_STEPS", DEFAULT_MAX_STEPS)?;
        let max_parallel_tools = parse_positive_usize(
            &lookup,
            "DSH_MAX_PARALLEL_TOOLS",
            DEFAULT_MAX_PARALLEL_TOOLS,
        )?;
        let max_retries = parse_u32(&lookup, "DSH_MAX_RETRIES", 2)?;
        let retry_base_ms = parse_u64(&lookup, "DSH_RETRY_BASE_MS", 500)?;
        let max_context_chars = parse_positive_usize(&lookup, "DSH_MAX_CONTEXT_CHARS", 200_000)?;

        let effort = match lookup("DSH_REASONING_EFFORT") {
            Some(raw) => ReasoningEffort::parse(&raw).ok_or(ConfigError::InvalidValue {
                key: "DSH_REASONING_EFFORT",
                value: raw,
            })?,
            None => ReasoningEffort::High,
        };

        let permission = match lookup("DSH_PERMISSION_MODE") {
            Some(raw) => PermissionMode::parse(&raw).ok_or(ConfigError::InvalidValue {
                key: "DSH_PERMISSION_MODE",
                value: raw,
            })?,
            None => PermissionMode::WorkspaceWrite,
        };

        let home = lookup("DSH_HOME").map_or_else(default_home, PathBuf::from);
        let system_prompt =
            lookup("DSH_SYSTEM_PROMPT").unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());

        Ok(Self {
            model,
            base_url,
            api_key_env,
            max_tokens,
            effort,
            max_steps,
            max_parallel_tools,
            max_retries,
            retry_base_ms,
            max_context_chars,
            permission,
            home,
            system_prompt,
        })
    }

    /// Resolve the API key from the configured environment variable.
    #[must_use]
    pub fn api_key(&self, env: &dyn Env) -> Option<String> {
        env.var(&self.api_key_env).filter(|key| !key.is_empty())
    }

    /// Path of the SQLite session database.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.home.join("sessions.sqlite3")
    }
}

fn default_home() -> PathBuf {
    dirs::data_local_dir().map_or_else(
        || PathBuf::from(".deepseacrab"),
        |directory| directory.join("deepseacrab"),
    )
}

fn parse_positive_u32<F>(lookup: &F, key: &'static str, default: u32) -> Result<u32, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(key) else {
        return Ok(default);
    };
    let value = raw
        .parse::<u32>()
        .map_err(|_| ConfigError::InvalidNumber { key })?;
    if value == 0 {
        return Err(ConfigError::InvalidNumber { key });
    }
    Ok(value)
}

fn parse_positive_usize<F>(
    lookup: &F,
    key: &'static str,
    default: usize,
) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(key) else {
        return Ok(default);
    };
    let value = raw
        .parse::<usize>()
        .map_err(|_| ConfigError::InvalidNumber { key })?;
    if value == 0 {
        return Err(ConfigError::InvalidNumber { key });
    }
    Ok(value)
}

fn parse_u32<F>(lookup: &F, key: &'static str, default: u32) -> Result<u32, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(key) else {
        return Ok(default);
    };
    raw.parse::<u32>()
        .map_err(|_| ConfigError::InvalidNumber { key })
}

fn parse_u64<F>(lookup: &F, key: &'static str, default: u64) -> Result<u64, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(key) else {
        return Ok(default);
    };
    raw.parse::<u64>()
        .map_err(|_| ConfigError::InvalidNumber { key })
}

/// Whether a string is an HTTP(S) root with a non-empty authority.
#[must_use]
pub fn valid_base_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }
    !url.chars().any(char::is_whitespace)
}

/// Configuration failures raised at the process boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// A numeric setting was not a positive integer.
    #[error("{key} must be a positive integer")]
    InvalidNumber {
        /// Environment key.
        key: &'static str,
    },
    /// A setting carried a value outside its vocabulary.
    #[error("{key} has an invalid value: {value}")]
    InvalidValue {
        /// Environment key.
        key: &'static str,
        /// Offending value.
        value: String,
    },
    /// The base URL is not an HTTP(S) root.
    #[error("base URL must start with http:// or https:// and name a host: {0}")]
    InvalidBaseUrl(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    proptest! {
        #[test]
        fn http_roots_with_hosts_are_valid(
            scheme in prop_oneof![Just("http"), Just("https")],
            host in "[a-z0-9.]{1,24}",
            path in prop_oneof![Just(String::new()), Just("/anthropic".to_string())],
        ) {
            let url = format!("{scheme}://{host}{path}");
            prop_assert!(valid_base_url(&url));
        }

        #[test]
        fn schemeless_roots_are_invalid(host in "[a-z0-9.]{1,24}") {
            prop_assert!(!valid_base_url(&host));
        }
    }

    #[test]
    fn defaults_apply_when_nothing_is_configured() {
        let config = config_from(&[]).expect("defaults");
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(config.max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(config.max_parallel_tools, DEFAULT_MAX_PARALLEL_TOOLS);
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.retry_base_ms, 500);
        assert_eq!(config.max_context_chars, 200_000);
        assert_eq!(config.effort, ReasoningEffort::High);
        assert_eq!(config.permission, PermissionMode::WorkspaceWrite);
        assert_eq!(config.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(config.system_prompt, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn environment_values_override_defaults() {
        let config = config_from(&[
            ("DEEPSEEK_MODEL", "deepseek-v4-pro"),
            ("DEEPSEEK_BASE_URL", "http://localhost:8080"),
            ("DSH_API_KEY_ENV", "MY_KEY"),
            ("DSH_MAX_TOKENS", "128"),
            ("DSH_MAX_STEPS", "3"),
            ("DSH_MAX_PARALLEL_TOOLS", "1"),
            ("DSH_MAX_RETRIES", "0"),
            ("DSH_RETRY_BASE_MS", "25"),
            ("DSH_MAX_CONTEXT_CHARS", "4096"),
            ("DSH_REASONING_EFFORT", "low"),
            ("DSH_PERMISSION_MODE", "read-only"),
            ("DSH_HOME", "/tmp/dsh-home"),
            ("DSH_SYSTEM_PROMPT", "custom"),
        ])
        .expect("configured");
        assert_eq!(config.model, "deepseek-v4-pro");
        assert_eq!(config.base_url, "http://localhost:8080");
        assert_eq!(config.api_key_env, "MY_KEY");
        assert_eq!(config.max_tokens, 128);
        assert_eq!(config.max_steps, 3);
        assert_eq!(config.max_parallel_tools, 1);
        assert_eq!(config.max_retries, 0);
        assert_eq!(config.retry_base_ms, 25);
        assert_eq!(config.max_context_chars, 4096);
        assert_eq!(config.effort, ReasoningEffort::Low);
        assert_eq!(config.permission, PermissionMode::ReadOnly);
        assert_eq!(config.home, PathBuf::from("/tmp/dsh-home"));
        assert_eq!(config.system_prompt, "custom");
    }

    #[test]
    fn invalid_numbers_are_rejected() {
        assert_eq!(
            config_from(&[("DSH_MAX_TOKENS", "0")]),
            Err(ConfigError::InvalidNumber {
                key: "DSH_MAX_TOKENS"
            })
        );
        assert_eq!(
            config_from(&[("DSH_MAX_STEPS", "-1")]),
            Err(ConfigError::InvalidNumber {
                key: "DSH_MAX_STEPS"
            })
        );
        assert_eq!(
            config_from(&[("DSH_MAX_PARALLEL_TOOLS", "many")]),
            Err(ConfigError::InvalidNumber {
                key: "DSH_MAX_PARALLEL_TOOLS"
            })
        );
        assert_eq!(
            config_from(&[("DSH_MAX_RETRIES", "-1")]),
            Err(ConfigError::InvalidNumber {
                key: "DSH_MAX_RETRIES"
            })
        );
        assert_eq!(
            config_from(&[("DSH_RETRY_BASE_MS", "later")]),
            Err(ConfigError::InvalidNumber {
                key: "DSH_RETRY_BASE_MS"
            })
        );
        assert_eq!(
            config_from(&[("DSH_MAX_CONTEXT_CHARS", "0")]),
            Err(ConfigError::InvalidNumber {
                key: "DSH_MAX_CONTEXT_CHARS"
            })
        );
    }

    #[test]
    fn invalid_vocabularies_are_rejected() {
        assert!(matches!(
            config_from(&[("DSH_REASONING_EFFORT", "extreme")]),
            Err(ConfigError::InvalidValue { .. })
        ));
        assert!(matches!(
            config_from(&[("DSH_PERMISSION_MODE", "root")]),
            Err(ConfigError::InvalidValue { .. })
        ));
        assert!(matches!(
            config_from(&[("DSH_API_KEY_ENV", " ")]),
            Err(ConfigError::InvalidValue { .. })
        ));
        assert!(matches!(
            config_from(&[("DEEPSEEK_MODEL", "")]),
            Err(ConfigError::InvalidValue { .. })
        ));
    }

    #[test]
    fn invalid_base_urls_are_rejected() {
        assert_eq!(
            config_from(&[("DEEPSEEK_BASE_URL", "ftp://example.com")]),
            Err(ConfigError::InvalidBaseUrl("ftp://example.com".to_string()))
        );
        assert!(matches!(
            config_from(&[("DEEPSEEK_BASE_URL", "https://")]),
            Err(ConfigError::InvalidBaseUrl(_))
        ));
        assert!(matches!(
            config_from(&[("DEEPSEEK_BASE_URL", "https://exa mple.com")]),
            Err(ConfigError::InvalidBaseUrl(_))
        ));
    }

    #[test]
    fn api_key_comes_from_the_configured_variable() {
        struct MapEnv(HashMap<String, String>);
        impl Env for MapEnv {
            fn var(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
        }
        let config = config_from(&[("DSH_API_KEY_ENV", "MY_KEY")]).expect("config");
        let env = MapEnv(HashMap::from([(
            "MY_KEY".to_string(),
            "secret".to_string(),
        )]));
        assert_eq!(config.api_key(&env).as_deref(), Some("secret"));
        let empty = MapEnv(HashMap::from([("MY_KEY".to_string(), String::new())]));
        assert_eq!(config.api_key(&empty), None);
        assert_eq!(config.api_key(&MapEnv(HashMap::new())), None);
        assert_eq!(config.database_path(), config.home.join("sessions.sqlite3"));
    }

    #[test]
    fn system_env_reads_process_variables() {
        let value = SystemEnv.var("PATH");
        assert!(value.as_deref().is_some_and(|path| !path.is_empty()));
        assert_ne!(value.as_deref(), Some("xyzzy"));
    }

    #[test]
    fn default_home_is_a_named_directory() {
        let home = default_home();
        assert!(!home.as_os_str().is_empty());
        assert_eq!(
            home.file_name().and_then(std::ffi::OsStr::to_str),
            Some("deepseacrab")
        );
    }

    #[test]
    fn default_base_url_is_valid() {
        assert!(valid_base_url(DEFAULT_BASE_URL));
        assert!(!valid_base_url("https:///path"));
    }
}
