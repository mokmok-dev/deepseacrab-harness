//! Plugin that loads SSH hosts and registers the `ssh_run` tool.

use crate::config::{self, SshHost};
use crate::error::SshError;
use crate::tool::SshTool;
use dsh_core::{Plugin, PluginContext, PluginError, SystemEnv};
use std::path::PathBuf;
use std::sync::Arc;

/// Loads `DSH_SSH_HOSTS` and contributes one `ssh_run` tool.
pub struct SshPlugin {
    /// Hosts already resolved from the environment.
    pub hosts: Vec<SshHost>,
    /// SSH executable to spawn.
    pub ssh: PathBuf,
}

impl SshPlugin {
    /// Read `DSH_SSH_HOSTS` from the process environment.
    ///
    /// A malformed value contributes no hosts here. [`Plugin::install`] reads
    /// the same variable from the supplied environment and reports the parse
    /// detail, so composition refuses a broken configuration.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            hosts: config::hosts_from_env(&SystemEnv).unwrap_or_default(),
            ssh: PathBuf::from(config::DEFAULT_SSH),
        }
    }

    /// Parse the same document [`SshPlugin::from_env`] accepts.
    pub fn parse_hosts(json: &str) -> Result<Vec<SshHost>, SshError> {
        config::parse_hosts(json)
    }
}

impl Plugin for SshPlugin {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let hosts = if self.hosts.is_empty() {
            config::hosts_from_env(context.env())
                .map_err(|error| PluginError::Failed(error.to_string()))?
        } else {
            self.hosts.clone()
        };
        if hosts.is_empty() {
            return Ok(());
        }
        context.add_tool(Arc::new(SshTool::new(hosts, self.ssh.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_core::{
        ComposeError, Config, Env, EventRecord, LlmError, LlmErrorCode, LlmProvider, LlmRequest,
        LlmStream, Runtime, SessionEvent, SessionHeader, SessionStore, SessionSummary, StoreError,
        compose, ids::SessionId,
    };
    use std::collections::HashMap;
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn with(key: &str, value: &str) -> Self {
            Self(HashMap::from([(key.to_string(), value.to_string())]))
        }
    }

    impl Env for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    struct NoStore;

    impl SessionStore for NoStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
            Ok(())
        }

        fn append(
            &self,
            _session_id: &SessionId,
            _at: i64,
            _event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            Err(StoreError::Backend("unused".to_string()))
        }

        fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
            Ok(Vec::new())
        }

        fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
            Ok(Vec::new())
        }

        fn has(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }

        fn delete(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    struct NoProvider;

    #[async_trait]
    impl LlmProvider for NoProvider {
        fn name(&self) -> &'static str {
            "none"
        }

        async fn stream(
            &self,
            _request: LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            Err(LlmError::new(LlmErrorCode::Transport, "unused", false))
        }
    }

    struct StorePlugin;

    impl Plugin for StorePlugin {
        fn name(&self) -> &'static str {
            "store"
        }

        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            context.set_store(Arc::new(NoStore))
        }
    }

    struct ProviderPlugin;

    impl Plugin for ProviderPlugin {
        fn name(&self) -> &'static str {
            "provider"
        }

        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            context.set_provider(Arc::new(NoProvider))
        }
    }

    fn compose_with(plugin: SshPlugin, env: &dyn Env) -> Result<Runtime, ComposeError> {
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(plugin),
        ];
        compose(&plugins, &Config::default(), env)
    }

    fn plugin(hosts: Vec<SshHost>) -> SshPlugin {
        SshPlugin {
            hosts,
            ssh: PathBuf::from("ssh"),
        }
    }

    fn web() -> SshHost {
        SshHost {
            name: "web".to_string(),
            host: "example.com".to_string(),
            user: None,
            port: None,
            identity: None,
        }
    }

    fn tool_names(runtime: &Runtime) -> Vec<String> {
        runtime.tools.names()
    }

    #[test]
    fn the_plugin_is_named_ssh() {
        assert_eq!(plugin(Vec::new()).name(), "ssh");
    }

    #[test]
    fn from_env_uses_the_system_client() {
        assert_eq!(SshPlugin::from_env().ssh, PathBuf::from("ssh"));
    }

    #[test]
    fn parse_hosts_accepts_the_documented_document() {
        assert_eq!(
            SshPlugin::parse_hosts(r#"[{"name":"web","host":"example.com"}]"#).expect("parses"),
            vec![web()]
        );
        assert!(matches!(
            SshPlugin::parse_hosts("{}"),
            Err(SshError::Config(_))
        ));
    }

    #[test]
    fn hosts_set_on_the_plugin_win_over_the_environment() {
        let env = MapEnv::with(config::HOSTS_ENV, "{");
        let runtime = compose_with(plugin(vec![web()]), &env).expect("composes");
        assert_eq!(tool_names(&runtime), vec!["ssh_run".to_string()]);
    }

    #[test]
    fn the_environment_variable_registers_the_tool() {
        let env = MapEnv::with(
            config::HOSTS_ENV,
            r#"[{"name":"web","host":"example.com"}]"#,
        );
        let runtime = compose_with(plugin(Vec::new()), &env).expect("composes");
        assert_eq!(tool_names(&runtime), vec!["ssh_run".to_string()]);
    }

    #[test]
    fn an_absent_or_blank_variable_registers_no_tool() {
        for env in [
            MapEnv::default(),
            MapEnv::with(config::HOSTS_ENV, ""),
            MapEnv::with(config::HOSTS_ENV, "  "),
            MapEnv::with(config::HOSTS_ENV, "[]"),
        ] {
            let runtime = compose_with(plugin(Vec::new()), &env).expect("composes");
            assert_eq!(tool_names(&runtime), Vec::<String>::new());
        }
    }

    #[test]
    fn malformed_configuration_fails_composition() {
        for raw in [
            "{",
            r#"[{"name":"","host":"example.com"}]"#,
            r#"[{"name":"web","host":"example.com"},{"name":"web","host":"other.example.com"}]"#,
        ] {
            let env = MapEnv::with(config::HOSTS_ENV, raw);
            let Err(error) = compose_with(plugin(Vec::new()), &env) else {
                panic!("document {raw:?} composed");
            };
            match error {
                ComposeError::Plugin {
                    plugin: "ssh",
                    error: PluginError::Failed(detail),
                } => assert!(
                    detail.contains("invalid ssh host configuration"),
                    "detail was {detail:?}"
                ),
                other => panic!("unexpected composition error: {other:?}"),
            }
        }
    }
}
