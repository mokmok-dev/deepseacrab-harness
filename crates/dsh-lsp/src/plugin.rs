//! Plugin that starts configured language servers and registers the `lsp` tool.

use crate::config::{self, LspServerConfig};
use crate::connection::{LspConnection, ProcessConnection};
use crate::error::LspError;
use crate::tool::{LspServer, LspTool};
use dsh_core::{Plugin, PluginContext, PluginError, SystemEnv};
use std::sync::Arc;

type Connector<'a> = dyn Fn(&LspServerConfig) -> Result<Arc<dyn LspConnection>, LspError> + 'a;

/// Starts each configured language server and contributes one `lsp` tool.
pub struct LspPlugin {
    /// Servers to start, in configuration order.
    pub servers: Vec<LspServerConfig>,
}

impl LspPlugin {
    /// Read `DSH_LSP_SERVERS` from the process environment.
    ///
    /// The variable holds a JSON array of
    /// `{name, command, args?, extensions}` objects. An absent, blank, or
    /// malformed value contributes no servers.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            servers: config::servers_from_env(&SystemEnv),
        }
    }

    /// Parse the same document [`LspPlugin::from_env`] accepts.
    pub fn parse_servers(json: &str) -> Result<Vec<LspServerConfig>, LspError> {
        config::parse_servers(json)
    }

    fn install_with(
        &self,
        context: &mut PluginContext<'_>,
        connect: &Connector<'_>,
    ) -> Result<(), PluginError> {
        let servers = if self.servers.is_empty() {
            config::servers_from_env(context.env())
        } else {
            self.servers.clone()
        };
        if servers.is_empty() {
            return Ok(());
        }
        let mut connected = Vec::with_capacity(servers.len());
        for server in &servers {
            let connection = connect(server).map_err(|error| server_failed(server, &error))?;
            connected.push(LspServer::new(server.clone(), connection));
        }
        context.add_tool(Arc::new(LspTool::new(connected)))
    }
}

fn server_failed(server: &LspServerConfig, error: &LspError) -> PluginError {
    PluginError::Failed(format!("lsp server {}: {error}", server.name))
}

impl Plugin for LspPlugin {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.install_with(context, &|server| {
            let connection: Arc<dyn LspConnection> = Arc::new(ProcessConnection::spawn(server)?);
            Ok(connection)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MapEnv, MockConnection, MockMode, compose_with, compose_with_env};
    use dsh_core::{ComposeError, Plugin, PluginContext};
    use serde_json::json;
    use std::ffi::OsStr;
    use std::process::Command;
    use std::sync::Mutex;

    struct Fixture {
        servers: Vec<LspServerConfig>,
        mode: MockMode,
        seen: Arc<Mutex<Vec<LspServerConfig>>>,
    }

    impl Fixture {
        fn new(servers: Vec<LspServerConfig>, mode: MockMode) -> Self {
            Self {
                servers,
                mode,
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Plugin for Fixture {
        fn name(&self) -> &'static str {
            "fixture"
        }

        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            let plugin = LspPlugin {
                servers: self.servers.clone(),
            };
            let mode = self.mode;
            let seen = Arc::clone(&self.seen);
            plugin.install_with(context, &move |server: &LspServerConfig| {
                seen.lock().expect("seen").push(server.clone());
                if mode == MockMode::ConnectFails {
                    return Err(LspError::Protocol("connect failed".to_string()));
                }
                let connection: Arc<dyn LspConnection> = Arc::new(MockConnection::new(mode));
                Ok(connection)
            })
        }
    }

    fn server(name: &str, extensions: &[&str]) -> LspServerConfig {
        LspServerConfig {
            name: name.to_string(),
            command: "mock-lsp".to_string(),
            args: Vec::new(),
            extensions: extensions.iter().map(ToString::to_string).collect(),
        }
    }

    fn json_servers(servers: &[LspServerConfig]) -> String {
        let document: Vec<_> = servers
            .iter()
            .map(|server| {
                json!({
                    "name": server.name,
                    "command": server.command,
                    "args": server.args,
                    "extensions": server.extensions,
                })
            })
            .collect();
        serde_json::Value::Array(document).to_string()
    }

    #[test]
    fn the_plugin_is_named_lsp() {
        assert_eq!(
            LspPlugin {
                servers: Vec::new()
            }
            .name(),
            "lsp"
        );
    }

    #[test]
    fn parse_servers_delegates_to_the_configuration_parser() {
        let servers =
            LspPlugin::parse_servers(r#"[{"name":"rust","command":"mock","extensions":["rs"]}]"#)
                .expect("parses");
        assert_eq!(
            servers,
            vec![LspServerConfig {
                name: "rust".to_string(),
                command: "mock".to_string(),
                args: Vec::new(),
                extensions: vec!["rs".to_string()],
            }]
        );
        assert_eq!(
            LspPlugin::parse_servers("[]").expect("empty"),
            Vec::<LspServerConfig>::new()
        );
        assert!(LspPlugin::parse_servers("{}").is_err());
    }

    #[test]
    fn no_servers_registers_no_tool() {
        let runtime =
            compose_with(Box::new(Fixture::new(Vec::new(), MockMode::Ok))).expect("compose");
        assert!(runtime.tools.is_empty());
    }

    #[test]
    fn configured_servers_register_one_tool() {
        let fixture = Fixture::new(
            vec![server("rust", &["rs"]), server("python", &["py"])],
            MockMode::Ok,
        );
        let runtime = compose_with(Box::new(fixture)).expect("compose");
        assert_eq!(runtime.tools.names(), vec!["lsp".to_string()]);
    }

    #[test]
    fn the_environment_supplies_servers_when_none_are_configured() {
        let expected = server("env-rust", &["rs"]);
        let env = MapEnv::with(
            config::SERVERS_ENV,
            &json_servers(std::slice::from_ref(&expected)),
        );
        let fixture = Fixture::new(Vec::new(), MockMode::Ok);
        let seen = Arc::clone(&fixture.seen);
        let runtime = compose_with_env(Box::new(fixture), &env).expect("compose");
        assert_eq!(runtime.tools.names(), vec!["lsp".to_string()]);
        assert_eq!(*seen.lock().expect("seen"), vec![expected]);
    }

    #[test]
    fn configured_servers_win_over_the_environment() {
        let configured = server("configured", &["rs"]);
        let env = MapEnv::with(
            config::SERVERS_ENV,
            &json_servers(&[server("env-rust", &["rs"])]),
        );
        let fixture = Fixture::new(vec![configured.clone()], MockMode::Ok);
        let seen = Arc::clone(&fixture.seen);
        compose_with_env(Box::new(fixture), &env).expect("compose");
        assert_eq!(*seen.lock().expect("seen"), vec![configured]);
    }

    #[test]
    fn a_connect_failure_names_the_server() {
        let error = compose_with(Box::new(Fixture::new(
            vec![server("broken", &["rs"])],
            MockMode::ConnectFails,
        )))
        .err()
        .expect("fails");
        match error {
            ComposeError::Plugin {
                plugin: "fixture",
                error: PluginError::Failed(message),
            } => {
                assert!(message.contains("broken"));
                assert!(message.contains("connect failed"));
            }
            other => panic!("expected a server connect failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_real_server_answers_the_registered_tool() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("sample.rs"), "fn main() {}\n").expect("write");
        let runtime = compose_with(Box::new(LspPlugin {
            servers: vec![crate::test_util::stub_server(directory.path(), "stub")],
        }))
        .expect("compose");
        let tool = runtime.tools.get("lsp").expect("registered");
        let context = dsh_core::ToolContext::new(
            directory.path().to_path_buf(),
            dsh_core::PermissionMode::ReadOnly,
            tokio_util::sync::CancellationToken::new(),
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tool.run(
                &context,
                r#"{"operation":"hover","path":"sample.rs","line":1,"character":1}"#,
            ),
        )
        .await
        .expect("tool timed out");
        assert_eq!(outcome.text_content(), "stub hover");
        assert!(!outcome.is_error);
    }

    #[test]
    fn from_env_probe_present() {
        if std::env::var_os("DSH_LSP_PROBE").as_deref() != Some(OsStr::new("present")) {
            return;
        }
        let plugin = LspPlugin::from_env();
        assert_eq!(plugin.servers, vec![server("probe", &["rs"])]);
        println!("from-env-probe-present-ok");
    }

    #[test]
    fn from_env_probe_absent() {
        if std::env::var_os("DSH_LSP_PROBE").as_deref() != Some(OsStr::new("absent")) {
            return;
        }
        assert_eq!(LspPlugin::from_env().servers, Vec::<LspServerConfig>::new());
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
            .env("DSH_LSP_PROBE", "present")
            .env(
                config::SERVERS_ENV,
                r#"[{"name":"probe","command":"mock-lsp","extensions":["rs"]}]"#,
            )
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
            .env("DSH_LSP_PROBE", "absent")
            .env_remove(config::SERVERS_ENV)
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
