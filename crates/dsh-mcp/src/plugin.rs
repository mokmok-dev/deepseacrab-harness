//! Plugin that starts configured MCP servers and registers their tools.

use crate::config::{self, McpServerConfig};
use crate::connection::{McpConnection, ProcessConnection};
use crate::error::McpError;
use crate::tool::McpTool;
use dsh_core::{Plugin, PluginContext, PluginError, SystemEnv};
use std::sync::Arc;

type Connector<'a> = dyn Fn(&McpServerConfig) -> Result<Arc<dyn McpConnection>, McpError> + 'a;

/// Starts each configured MCP server and contributes one tool per declaration.
pub struct McpPlugin {
    /// Servers to start, in configuration order.
    pub servers: Vec<McpServerConfig>,
}

impl McpPlugin {
    /// Read `DSH_MCP_SERVERS` from the process environment.
    ///
    /// The variable holds a JSON array of `{name, command, args?, env?}`
    /// objects. An absent, blank, or malformed value contributes no servers.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            servers: config::servers_from_env(&SystemEnv),
        }
    }

    /// Parse the same document [`McpPlugin::from_env`] accepts.
    pub fn parse_servers(json: &str) -> Result<Vec<McpServerConfig>, McpError> {
        config::parse_servers(json)
    }

    fn install_with(
        &self,
        context: &mut PluginContext<'_>,
        connect: &Connector<'_>,
    ) -> Result<(), PluginError> {
        for server in &self.servers {
            let connection = connect(server).map_err(|error| server_failed(server, &error))?;
            let specs = connection
                .list_tools()
                .map_err(|error| server_failed(server, &error))?;
            for spec in specs {
                let tool = McpTool::new(server.name.clone(), spec, Arc::clone(&connection));
                context.add_tool(Arc::new(tool))?;
            }
        }
        Ok(())
    }
}

fn server_failed(server: &McpServerConfig, error: &McpError) -> PluginError {
    PluginError::Failed(format!("mcp server {}: {error}", server.name))
}

impl Plugin for McpPlugin {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.install_with(context, &|server| {
            let connection: Arc<dyn McpConnection> = Arc::new(ProcessConnection::spawn(server)?);
            Ok(connection)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{
        MockConnection, MockMode, compose_with, spec, stub_server, tool_context,
    };
    use dsh_core::{ComposeError, PluginContext, PluginError};
    use std::ffi::OsStr;
    use std::process::Command;

    struct Fixture {
        servers: Vec<McpServerConfig>,
        mode: MockMode,
    }

    impl Plugin for Fixture {
        fn name(&self) -> &'static str {
            "fixture"
        }

        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            let plugin = McpPlugin {
                servers: self.servers.clone(),
            };
            let mode = self.mode;
            plugin.install_with(context, &move |_server: &McpServerConfig| {
                if mode == MockMode::ConnectFails {
                    return Err(McpError::Protocol("connect failed".to_string()));
                }
                let connection: Arc<dyn McpConnection> =
                    Arc::new(MockConnection::new(vec![spec("echo"), spec("ping")], mode));
                Ok(connection)
            })
        }
    }

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "mock".to_string(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    #[test]
    fn the_plugin_is_named_mcp() {
        assert_eq!(
            McpPlugin {
                servers: Vec::new()
            }
            .name(),
            "mcp"
        );
    }

    #[test]
    fn parse_servers_delegates_to_the_configuration_parser() {
        let servers =
            McpPlugin::parse_servers(r#"[{"name":"a","command":"mock"}]"#).expect("parses");
        assert_eq!(servers, vec![server("a")]);
        assert_eq!(
            McpPlugin::parse_servers("[]").expect("empty"),
            Vec::<McpServerConfig>::new()
        );
        assert!(McpPlugin::parse_servers("{}").is_err());
    }

    #[test]
    fn one_tool_is_registered_per_declaration() {
        let runtime = compose_with(Box::new(Fixture {
            servers: vec![server("stub")],
            mode: MockMode::Ok,
        }))
        .expect("compose");
        assert_eq!(
            runtime.tools.names(),
            vec!["mcp_stub_echo".to_string(), "mcp_stub_ping".to_string()]
        );
    }

    #[test]
    fn every_server_namespaces_its_own_tools() {
        let runtime = compose_with(Box::new(Fixture {
            servers: vec![server("alpha"), server("beta")],
            mode: MockMode::Ok,
        }))
        .expect("compose");
        assert_eq!(
            runtime.tools.names(),
            vec![
                "mcp_alpha_echo".to_string(),
                "mcp_alpha_ping".to_string(),
                "mcp_beta_echo".to_string(),
                "mcp_beta_ping".to_string(),
            ]
        );
    }

    #[test]
    fn a_listing_failure_names_the_server() {
        let error = compose_with(Box::new(Fixture {
            servers: vec![server("stub")],
            mode: MockMode::ListFails,
        }))
        .err()
        .expect("fails");
        match error {
            ComposeError::Plugin {
                plugin: "fixture",
                error: PluginError::Failed(message),
            } => {
                assert!(message.contains("stub"));
                assert!(message.contains("list failed"));
            }
            other => panic!("expected a server listing failure, got {other:?}"),
        }
    }

    #[test]
    fn a_connect_failure_names_the_server() {
        let error = compose_with(Box::new(Fixture {
            servers: vec![server("stub")],
            mode: MockMode::ConnectFails,
        }))
        .err()
        .expect("fails");
        match error {
            ComposeError::Plugin {
                plugin: "fixture",
                error: PluginError::Failed(message),
            } => {
                assert!(message.contains("stub"));
                assert!(message.contains("connect failed"));
            }
            other => panic!("expected a server connect failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_configured_server_registers_and_executes_its_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut config = stub_server(directory.path(), "stub");
        config
            .env
            .push(("STUB_TEXT".to_string(), "plugin ".to_string()));
        let runtime = compose_with(Box::new(McpPlugin {
            servers: vec![config],
        }))
        .expect("compose");
        assert_eq!(runtime.tools.names(), vec!["mcp_stub_echo".to_string()]);

        let tool = runtime.tools.get("mcp_stub_echo").expect("registered");
        let outcome = tool.run(&tool_context(), r#"{"text":"hi"}"#).await;
        assert_eq!(outcome.text_content(), "plugin result");
        assert!(!outcome.is_error);
    }

    #[test]
    fn from_env_probe_present() {
        if std::env::var_os("DSH_MCP_PROBE").as_deref() != Some(OsStr::new("present")) {
            return;
        }
        let plugin = McpPlugin::from_env();
        assert_eq!(plugin.servers, vec![server("probe")]);
        println!("from-env-probe-present-ok");
    }

    #[test]
    fn from_env_probe_absent() {
        if std::env::var_os("DSH_MCP_PROBE").as_deref() != Some(OsStr::new("absent")) {
            return;
        }
        assert_eq!(McpPlugin::from_env().servers, Vec::<McpServerConfig>::new());
        println!("from-env-probe-absent-ok");
    }

    #[test]
    fn from_env_reads_the_process_environment() {
        let executable = std::env::current_exe().expect("current executable");
        let present = Command::new(&executable)
            .args([
                "--exact",
                "plugin::tests::from_env_probe_present",
                "--nocapture",
            ])
            .env("DSH_MCP_PROBE", "present")
            .env("DSH_MCP_SERVERS", r#"[{"name":"probe","command":"mock"}]"#)
            .output()
            .expect("spawn probe");
        assert!(present.status.success());
        assert!(
            String::from_utf8_lossy(&present.stdout).contains("from-env-probe-present-ok"),
            "present probe did not run: {}",
            String::from_utf8_lossy(&present.stdout)
        );

        let absent = Command::new(&executable)
            .args([
                "--exact",
                "plugin::tests::from_env_probe_absent",
                "--nocapture",
            ])
            .env("DSH_MCP_PROBE", "absent")
            .env_remove("DSH_MCP_SERVERS")
            .output()
            .expect("spawn probe");
        assert!(absent.status.success());
        assert!(
            String::from_utf8_lossy(&absent.stdout).contains("from-env-probe-absent-ok"),
            "absent probe did not run: {}",
            String::from_utf8_lossy(&absent.stdout)
        );
    }
}
