//! Configured MCP servers parsed from the environment.

use crate::error::McpError;
use dsh_core::Env;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Environment variable holding the JSON server list.
pub const SERVERS_ENV: &str = "DSH_MCP_SERVERS";

/// One stdio MCP server the harness may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Stable server name used to namespace its tools.
    pub name: String,
    /// Executable to spawn.
    pub command: String,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Extra environment entries merged over the inherited environment.
    pub env: Vec<(String, String)>,
}

/// The document shape accepted by [`parse_servers`].
#[derive(Debug, Deserialize)]
struct RawServerConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Parse a JSON array of server objects.
///
/// `args` and `env` are optional. `name` and `command` must be present and
/// non-blank.
pub fn parse_servers(json: &str) -> Result<Vec<McpServerConfig>, McpError> {
    let raw: Vec<RawServerConfig> =
        serde_json::from_str(json).map_err(|error| McpError::Config(error.to_string()))?;
    raw.into_iter().map(server_from_raw).collect()
}

fn server_from_raw(raw: RawServerConfig) -> Result<McpServerConfig, McpError> {
    if raw.name.trim().is_empty() {
        return Err(McpError::Config(
            "server name must not be blank".to_string(),
        ));
    }
    if raw.command.trim().is_empty() {
        return Err(McpError::Config(format!(
            "server {} has a blank command",
            raw.name
        )));
    }
    Ok(McpServerConfig {
        name: raw.name,
        command: raw.command,
        args: raw.args,
        env: raw.env.into_iter().collect(),
    })
}

/// Servers configured through `DSH_MCP_SERVERS`.
///
/// An absent, blank, or malformed value yields no servers so a broken optional
/// integration does not block the rest of composition.
pub fn servers_from_env(env: &dyn Env) -> Vec<McpServerConfig> {
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

    fn parse(value: &serde_json::Value) -> Result<Vec<McpServerConfig>, McpError> {
        parse_servers(&value.to_string())
    }

    #[test]
    fn required_fields_flow_into_the_config() {
        let servers = parse(&json!([{
            "name": "files",
            "command": "npx",
            "args": ["-y", "files-server"],
            "env": {"TOKEN": "secret"}
        }]))
        .expect("parses");
        assert_eq!(
            servers,
            vec![McpServerConfig {
                name: "files".to_string(),
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "files-server".to_string()],
                env: vec![("TOKEN".to_string(), "secret".to_string())],
            }]
        );
    }

    #[test]
    fn optional_fields_default_to_empty() {
        let servers =
            parse(&json!([{"name": "files", "command": "files-server"}])).expect("parses");
        assert_eq!(
            servers,
            vec![McpServerConfig {
                name: "files".to_string(),
                command: "files-server".to_string(),
                args: Vec::new(),
                env: Vec::new(),
            }]
        );
    }

    #[test]
    fn environment_entries_are_sorted() {
        let servers = parse(&json!([{
            "name": "files",
            "command": "files-server",
            "env": {"ZED": "1", "ALPHA": "2"}
        }]))
        .expect("parses");
        assert_eq!(
            servers[0].env,
            vec![
                ("ALPHA".to_string(), "2".to_string()),
                ("ZED".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        assert!(parse(&json!([{"command": "files-server"}])).is_err());
        assert!(parse(&json!([{"name": "files"}])).is_err());
    }

    #[test]
    fn wrong_document_shapes_are_rejected() {
        assert!(parse_servers("").is_err());
        assert!(parse_servers("{}").is_err());
        assert!(parse_servers("null").is_err());
        assert!(parse(&json!([{"name": "files", "command": 7}])).is_err());
        assert!(parse(&json!([{"name": "files", "command": "x", "args": "nope"}])).is_err());
        assert!(parse(&json!([{"name": "files", "command": "x", "env": []}])).is_err());
    }

    #[test]
    fn blank_name_or_command_is_rejected() {
        assert_eq!(
            parse(&json!([{"name": " ", "command": "x"}])),
            Err(McpError::Config(
                "server name must not be blank".to_string()
            ))
        );
        assert_eq!(
            parse(&json!([{"name": "server", "command": ""}])),
            Err(McpError::Config(
                "server server has a blank command".to_string()
            ))
        );
    }

    #[test]
    fn a_blank_environment_value_yields_no_servers() {
        let empty = Vec::<McpServerConfig>::new();
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
            r#"[{"name":"a","command":"b"}]"#,
        ));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "a");
        assert_eq!(servers[0].command, "b");
    }

    proptest! {
        #[test]
        fn objects_with_required_fields_parse(
            name in "[A-Za-z0-9_-]{1,16}",
            command in "[A-Za-z0-9_./-]{1,24}",
            args in prop::collection::vec("[ -~]{0,16}", 0..4),
            env in prop::collection::btree_map("[A-Z_]{1,8}", "[ -~]{0,16}", 0..4),
        ) {
            let document = json!([{
                "name": name,
                "command": command,
                "args": args,
                "env": env,
            }]);
            let servers = parse(&document).expect("parses");
            prop_assert_eq!(servers.len(), 1);
            prop_assert_eq!(&servers[0].name, &name);
            prop_assert_eq!(&servers[0].command, &command);
            prop_assert_eq!(&servers[0].args, &args);
            prop_assert_eq!(servers[0].env.len(), env.len());
            for ((key, value), (expected_key, expected_value)) in servers[0].env.iter().zip(&env) {
                prop_assert_eq!(key, expected_key);
                prop_assert_eq!(value, expected_value);
            }
        }

        #[test]
        fn missing_required_fields_are_always_rejected(
            name in "[A-Za-z0-9_-]{1,16}",
            command in "[A-Za-z0-9_./-]{1,24}",
        ) {
            let without_command = json!([{"command": command}]);
            let without_name = json!([{"name": name}]);
            prop_assert!(parse(&without_command).is_err());
            prop_assert!(parse(&without_name).is_err());
        }
    }
}
