//! Plugin composition with reversible registration effects.
//!
//! A plugin installs capabilities into a [`Registry`] and records one undo
//! effect per contribution. A later plugin failure unwinds those effects in
//! reverse order, so composition either produces a complete runtime or leaves
//! no partial surface behind.

use crate::config::{Config, Env};
use crate::llm::LlmProvider;
use crate::store::SessionStore;
use crate::tool::{Tool, ToolRegistry, ToolRegistryError};
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;

/// Wall clock used by durable writes.
pub trait Clock: Send + Sync {
    /// Unix epoch milliseconds.
    fn now_millis(&self) -> i64;
}

/// Process clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        Utc::now().timestamp_millis()
    }
}

/// Fixed clock for deterministic tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0
    }
}

/// One undo action for a registry contribution.
pub type Effect = Box<dyn FnOnce(&mut Registry) + Send>;

/// Capabilities contributed during composition.
#[derive(Default)]
pub struct Registry {
    store: Option<Arc<dyn SessionStore>>,
    provider: Option<Arc<dyn LlmProvider>>,
    tools: ToolRegistry,
}

/// Registration failures raised by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginError {
    /// The single-valued service already has an owner.
    #[error("service {0} is already registered")]
    DuplicateService(&'static str),
    /// A tool name is already claimed.
    #[error(transparent)]
    Tool(#[from] ToolRegistryError),
    /// The plugin could not construct its contribution.
    #[error("plugin setup failed: {0}")]
    Failed(String),
}

/// The mutable view a plugin installs through.
pub struct PluginContext<'a> {
    registry: &'a mut Registry,
    effects: &'a mut Vec<Effect>,
    config: &'a Config,
    env: &'a dyn Env,
}

impl PluginContext<'_> {
    /// The validated deployment configuration.
    #[must_use]
    pub const fn config(&self) -> &Config {
        self.config
    }

    /// Environment reads, including credential lookups.
    #[must_use]
    pub const fn env(&self) -> &dyn Env {
        self.env
    }

    /// Register the session store.
    pub fn set_store(&mut self, store: Arc<dyn SessionStore>) -> Result<(), PluginError> {
        if self.registry.store.is_some() {
            return Err(PluginError::DuplicateService("session store"));
        }
        self.registry.store = Some(store);
        self.effects
            .push(Box::new(|registry| registry.store = None));
        Ok(())
    }

    /// Register the language model provider.
    pub fn set_provider(&mut self, provider: Arc<dyn LlmProvider>) -> Result<(), PluginError> {
        if self.registry.provider.is_some() {
            return Err(PluginError::DuplicateService("language model provider"));
        }
        self.registry.provider = Some(provider);
        self.effects
            .push(Box::new(|registry| registry.provider = None));
        Ok(())
    }

    /// Register one model-facing tool.
    pub fn add_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), PluginError> {
        let name = tool.schema().name;
        self.registry.tools.insert(tool)?;
        self.effects.push(Box::new(move |registry| {
            registry.tools.remove(&name);
        }));
        Ok(())
    }

    /// Record an arbitrary undo action for a contribution this context has no
    /// typed setter for.
    pub fn on_dispose(&mut self, effect: impl FnOnce(&mut Registry) + Send + 'static) {
        self.effects.push(Box::new(effect));
    }
}

/// A unit of composition.
pub trait Plugin: Send + Sync {
    /// Stable plugin name used in diagnostics.
    fn name(&self) -> &'static str;

    /// Contribute capabilities. Every contribution records its undo effect.
    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError>;
}

/// Composition failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ComposeError {
    /// A plugin rejected composition.
    #[error("plugin {plugin} failed: {error}")]
    Plugin {
        /// Failing plugin.
        plugin: &'static str,
        /// Registration failure.
        error: PluginError,
    },
    /// Composition finished without a required capability.
    #[error("composition is missing a {0}")]
    MissingService(&'static str),
}

/// The complete runtime surface produced by a plugin set.
pub struct Runtime {
    /// Durable session storage.
    pub store: Arc<dyn SessionStore>,
    /// Model route.
    pub provider: Arc<dyn LlmProvider>,
    /// Registered tools.
    pub tools: ToolRegistry,
}

/// Apply plugins in order and require a store and a provider.
///
/// On failure every recorded effect runs in reverse order before the error is
/// returned.
pub fn compose(
    plugins: &[Box<dyn Plugin>],
    config: &Config,
    env: &dyn Env,
) -> Result<Runtime, ComposeError> {
    let mut registry = Registry::default();
    let mut effects: Vec<Effect> = Vec::new();

    for plugin in plugins {
        let mut context = PluginContext {
            registry: &mut registry,
            effects: &mut effects,
            config,
            env,
        };
        if let Err(error) = plugin.install(&mut context) {
            rollback(&mut registry, effects);
            return Err(ComposeError::Plugin {
                plugin: plugin.name(),
                error,
            });
        }
    }

    let Some(store) = registry.store.take() else {
        rollback(&mut registry, effects);
        return Err(ComposeError::MissingService("session store"));
    };
    let Some(provider) = registry.provider.take() else {
        rollback(&mut registry, effects);
        return Err(ComposeError::MissingService("language model provider"));
    };

    Ok(Runtime {
        store,
        provider,
        tools: registry.tools,
    })
}

fn rollback(registry: &mut Registry, effects: Vec<Effect>) {
    for effect in effects.into_iter().rev() {
        effect(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventRecord, SessionEvent};
    use crate::ids::SessionId;
    use crate::llm::{LlmError, LlmRequest, LlmStream};
    use crate::session::{SessionHeader, SessionSummary};
    use crate::store::StoreError;
    use crate::tool::{EchoTool, ToolContext, ToolOutcome, ToolSchema};
    use async_trait::async_trait;
    use futures_util::stream;
    use serde_json::Value;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_util::sync::CancellationToken;

    struct MockStore;

    impl SessionStore for MockStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
            Ok(())
        }
        fn append(
            &self,
            _session_id: &SessionId,
            at: i64,
            event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            Ok(EventRecord {
                seq: crate::ids::EventSeq::new(0),
                at,
                event,
            })
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

    struct MockProvider;

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        async fn stream(
            &self,
            _request: LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<LlmStream, LlmError> {
            Ok(Box::pin(stream::empty()))
        }
    }

    struct StorePlugin;
    impl Plugin for StorePlugin {
        fn name(&self) -> &'static str {
            "store"
        }
        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            context.set_store(Arc::new(MockStore))
        }
    }

    struct ProviderPlugin;
    impl Plugin for ProviderPlugin {
        fn name(&self) -> &'static str {
            "provider"
        }
        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            context.set_provider(Arc::new(MockProvider))
        }
    }

    struct ToolPlugin;
    impl Plugin for ToolPlugin {
        fn name(&self) -> &'static str {
            "tools"
        }
        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            context.add_tool(Arc::new(EchoTool {
                name: "echo".to_string(),
            }))
        }
    }

    struct FailingPlugin;
    impl Plugin for FailingPlugin {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn install(&self, _context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            Err(PluginError::DuplicateService("session store"))
        }
    }

    struct FlagPlugin {
        flag: Arc<AtomicBool>,
    }
    impl Plugin for FlagPlugin {
        fn name(&self) -> &'static str {
            "flag"
        }
        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            context.set_store(Arc::new(MockStore))?;
            let flag = Arc::clone(&self.flag);
            context.on_dispose(move |_registry| flag.store(true, Ordering::SeqCst));
            Ok(())
        }
    }

    struct OrderPlugin {
        log: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Plugin for OrderPlugin {
        fn name(&self) -> &'static str {
            "order"
        }
        fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            let first = Arc::clone(&self.log);
            context.on_dispose(move |_registry| first.lock().expect("log").push("first"));
            let second = Arc::clone(&self.log);
            context.on_dispose(move |_registry| second.lock().expect("log").push("second"));
            Ok(())
        }
    }

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn composition_returns_every_capability() {
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(ToolPlugin),
        ];
        let runtime = compose(&plugins, &config(), &crate::config::SystemEnv).expect("compose");
        assert_eq!(runtime.provider.name(), "mock");
        assert_eq!(runtime.tools.names(), vec!["echo".to_string()]);
    }

    #[test]
    fn duplicate_services_are_rejected() {
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
        ];
        assert_eq!(
            compose(&plugins, &config(), &crate::config::SystemEnv)
                .err()
                .expect("duplicate"),
            ComposeError::Plugin {
                plugin: "store",
                error: PluginError::DuplicateService("session store"),
            }
        );
    }

    #[test]
    fn missing_provider_is_rejected() {
        let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(StorePlugin)];
        assert_eq!(
            compose(&plugins, &config(), &crate::config::SystemEnv)
                .err()
                .expect("missing"),
            ComposeError::MissingService("language model provider")
        );
    }

    #[test]
    fn missing_store_is_rejected() {
        let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(ProviderPlugin)];
        assert_eq!(
            compose(&plugins, &config(), &crate::config::SystemEnv)
                .err()
                .expect("missing"),
            ComposeError::MissingService("session store")
        );
    }

    #[test]
    fn failure_unwinds_registered_effects() {
        let flag = Arc::new(AtomicBool::new(false));
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(FlagPlugin {
                flag: Arc::clone(&flag),
            }),
            Box::new(FailingPlugin),
        ];
        assert!(compose(&plugins, &config(), &crate::config::SystemEnv).is_err());
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn rollback_runs_effects_in_reverse_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(OrderPlugin {
                log: Arc::clone(&log),
            }),
            Box::new(FailingPlugin),
        ];
        assert!(compose(&plugins, &config(), &crate::config::SystemEnv).is_err());
        assert_eq!(*log.lock().expect("log"), vec!["second", "first"]);
    }

    #[test]
    fn duplicate_tools_are_rejected() {
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(ToolPlugin),
            Box::new(ToolPlugin),
        ];
        assert!(matches!(
            compose(&plugins, &config(), &crate::config::SystemEnv),
            Err(ComposeError::Plugin {
                plugin: "tools",
                error: PluginError::Tool(ToolRegistryError::Duplicate(_)),
            })
        ));
    }

    #[test]
    fn clocks_report_their_value() {
        assert_eq!(FixedClock(99).now_millis(), 99);
        assert!(SystemClock.now_millis() > 1_000_000_000_000);
    }

    struct ConstantTool(ToolOutcome);
    #[async_trait]
    impl Tool for ConstantTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("constant", "c", serde_json::json!({})).expect("schema")
        }
        async fn execute(&self, _context: &ToolContext, _arguments: Value) -> ToolOutcome {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn tool_outcomes_are_observable_through_the_registry() {
        let mut registry = ToolRegistry::new();
        registry
            .insert(Arc::new(ConstantTool(ToolOutcome::text("observed"))))
            .expect("insert");
        let tool = registry.get("constant").expect("present");
        let outcome = tool
            .run(
                &ToolContext::new(
                    std::env::temp_dir(),
                    crate::tool::PermissionMode::ReadOnly,
                    CancellationToken::new(),
                ),
                "{}",
            )
            .await;
        assert_eq!(outcome.text_content(), "observed");
    }
}
