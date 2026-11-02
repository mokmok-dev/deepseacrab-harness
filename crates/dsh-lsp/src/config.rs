//! Configured language servers parsed from the environment.

use crate::error::LspError;
use dsh_core::Env;
use serde::Deserialize;

/// Environment variable holding the JSON server list.
pub const SERVERS_ENV: &str = "DSH_LSP_SERVERS";

/// One stdio language server the harness may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServerConfig {
    /// Stable server name used in diagnostics.
    pub name: String,
    /// Executable to spawn.
    pub command: String,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// File extensions the server handles, without the leading dot.
    pub extensions: Vec<String>,
}

/// The document shape accepted by [`parse_servers`].
#[derive(Debug, Deserialize)]
struct RawServerConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    extensions: Vec<String>,
}

/// Parse a JSON array of server objects.
///
/// `args` is optional. `name`, `command`, and `extensions` must be present;
/// names and commands must not be blank and every extension must be simple.
pub fn parse_servers(json: &str) -> Result<Vec<LspServerConfig>, LspError> {
    let raw: Vec<RawServerConfig> =
        serde_json::from_str(json).map_err(|error| LspError::Config(error.to_string()))?;
    raw.into_iter().map(server_from_raw).collect()
}

fn server_from_raw(raw: RawServerConfig) -> Result<LspServerConfig, LspError> {
    if raw.name.trim().is_empty() {
        return Err(LspError::Config(
            "server name must not be blank".to_string(),
        ));
    }
    if raw.command.trim().is_empty() {
        return Err(LspError::Config(format!(
            "server {} has a blank command",
            raw.name
        )));
    }
    if raw.extensions.is_empty() {
        return Err(LspError::Config(format!(
            "server {} must declare at least one extension",
            raw.name
        )));
    }
    for extension in &raw.extensions {
        if !is_valid_extension(extension) {
            return Err(LspError::Config(format!(
                "server {} extension {extension:?} must be simple, without a dot, slash, or space",
                raw.name
            )));
        }
    }
    Ok(LspServerConfig {
        name: raw.name,
        command: raw.command,
        args: raw.args,
        extensions: raw.extensions,
    })
}

/// Whether an extension is a bare suffix such as `rs` or `tsx`.
#[must_use]
pub fn is_valid_extension(extension: &str) -> bool {
    !extension.is_empty()
        && !extension.contains(['.', '/', '\\'])
        && !extension.chars().any(char::is_whitespace)
}

/// Servers configured through `DSH_LSP_SERVERS`.
///
/// An absent, blank, or malformed value yields no servers so a broken optional
/// integration does not block the rest of composition.
#[must_use]
pub fn servers_from_env(env: &dyn Env) -> Vec<LspServerConfig> {
    env.var(SERVERS_ENV)
        .filter(|raw| !raw.trim().is_empty())
        .and_then(|raw| parse_servers(&raw).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::MapEnv;
    use proptest::prelude::*;
    use serde_json::json;

    fn parse(value: &serde_json::Value) -> Result<Vec<LspServerConfig>, LspError> {
        parse_servers(&value.to_string())
    }

    fn server(name: &str) -> LspServerConfig {
        LspServerConfig {
            name: name.to_string(),
            command: "rust-analyzer".to_string(),
            args: Vec::new(),
            extensions: vec!["rs".to_string()],
        }
    }

    #[test]
    fn required_fields_flow_into_the_config() {
        let servers = parse(&json!([{
            "name": "rust",
            "command": "rust-analyzer",
            "args": ["--stdio"],
            "extensions": ["rs"]
        }]))
        .expect("parses");
        assert_eq!(
            servers,
            vec![LspServerConfig {
                name: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: vec!["rs".to_string()],
            }]
        );
    }

    #[test]
    fn optional_args_default_to_empty() {
        let servers =
            parse(&json!([{"name": "rust", "command": "rust-analyzer", "extensions": ["rs"]}]))
                .expect("parses");
        assert_eq!(servers, vec![server("rust")]);
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        assert!(parse(&json!([{"command": "x", "extensions": ["rs"]}])).is_err());
        assert!(parse(&json!([{"name": "rust", "extensions": ["rs"]}])).is_err());
        assert!(parse(&json!([{"name": "rust", "command": "x"}])).is_err());
    }

    #[test]
    fn wrong_document_shapes_are_rejected() {
        assert!(parse_servers("").is_err());
        assert!(parse_servers("{}").is_err());
        assert!(parse_servers("null").is_err());
        assert!(parse(&json!([{"name": "rust", "command": 7, "extensions": ["rs"]}])).is_err());
        assert!(
            parse(&json!([{"name": "rust", "command": "x", "args": "nope", "extensions": ["rs"]}]))
                .is_err()
        );
        assert!(parse(&json!([{"name": "rust", "command": "x", "extensions": "rs"}])).is_err());
    }

    #[test]
    fn blank_name_or_command_is_rejected() {
        assert_eq!(
            parse(&json!([{"name": " ", "command": "x", "extensions": ["rs"]}])),
            Err(LspError::Config(
                "server name must not be blank".to_string()
            ))
        );
        assert_eq!(
            parse(&json!([{"name": "server", "command": "", "extensions": ["rs"]}])),
            Err(LspError::Config(
                "server server has a blank command".to_string()
            ))
        );
    }

    #[test]
    fn an_empty_extension_list_is_rejected() {
        assert_eq!(
            parse(&json!([{"name": "rust", "command": "x", "extensions": []}])),
            Err(LspError::Config(
                "server rust must declare at least one extension".to_string()
            ))
        );
    }

    #[test]
    fn malformed_extensions_are_rejected() {
        for extension in ["", "a.rs", "a/b", "a\\b", "a b"] {
            let result = parse(&json!([{
                "name": "rust",
                "command": "x",
                "extensions": [extension]
            }]));
            assert_eq!(
                result,
                Err(LspError::Config(format!(
                    "server rust extension {extension:?} must be simple, without a dot, slash, or space"
                ))),
                "extension {extension:?} was accepted"
            );
        }
    }

    #[test]
    fn every_extension_in_a_server_is_validated() {
        assert!(
            parse(&json!([{"name": "s", "command": "c", "extensions": ["rs", "a/b"]}])).is_err()
        );
    }

    #[test]
    fn a_blank_environment_value_yields_no_servers() {
        let empty = Vec::<LspServerConfig>::new();
        assert_eq!(servers_from_env(&MapEnv::default()), empty);
        assert_eq!(servers_from_env(&MapEnv::with(SERVERS_ENV, "")), empty);
        assert_eq!(servers_from_env(&MapEnv::with(SERVERS_ENV, "   ")), empty);
        assert_eq!(servers_from_env(&MapEnv::with(SERVERS_ENV, "{")), empty);
        assert_eq!(servers_from_env(&MapEnv::with("OTHER", "[]")), empty);
    }

    #[test]
    fn the_environment_value_is_parsed() {
        let servers = servers_from_env(&MapEnv::with(
            SERVERS_ENV,
            r#"[{"name":"rust","command":"x","extensions":["rs"]}]"#,
        ));
        assert_eq!(
            servers,
            vec![LspServerConfig {
                name: "rust".to_string(),
                command: "x".to_string(),
                args: Vec::new(),
                extensions: vec!["rs".to_string()],
            }]
        );
    }

    proptest! {
        #[test]
        fn extension_validity_matches_the_rule(raw in "[ -~]{0,8}") {
            let expected = !raw.is_empty()
                && !raw.contains('.')
                && !raw.contains('/')
                && !raw.contains('\\')
                && !raw.contains(' ');
            prop_assert_eq!(is_valid_extension(&raw), expected);
        }

        #[test]
        fn simple_servers_always_parse(
            name in "[A-Za-z0-9_-]{1,12}",
            command in "[A-Za-z0-9_./-]{1,16}",
            args in prop::collection::vec("[ -~]{0,12}", 0..3),
            extensions in prop::collection::vec("[A-Za-z0-9_+-]{1,6}", 1..3),
        ) {
            let document = json!([{
                "name": name,
                "command": command,
                "args": args,
                "extensions": extensions,
            }]);
            let servers = parse(&document).expect("parses");
            prop_assert_eq!(servers.len(), 1);
            prop_assert_eq!(&servers[0].name, &name);
            prop_assert_eq!(&servers[0].command, &command);
            prop_assert_eq!(&servers[0].args, &args);
            prop_assert_eq!(&servers[0].extensions, &extensions);
        }
    }
}
