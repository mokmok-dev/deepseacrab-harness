#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Built-in model-facing tools and the plugin that registers them.
//!
//! Every filesystem tool resolves its path through [`dsh_core::ToolContext`],
//! so the session permission mode decides what may be read, written, or run.
//! Failures are returned as [`dsh_core::ToolOutcome::error`] so the agent loop
//! can always settle the matching tool result.

mod editing;
mod http;
mod input;
mod listing;
mod reading;
mod searching;
mod shell;
mod writing;

#[cfg(test)]
mod test_util;

use dsh_core::{Plugin, PluginContext, PluginError, Tool, ToolRegistry, ToolRegistryError};
use std::sync::Arc;

/// Registers every built-in tool with the composed runtime.
pub struct ToolsPlugin;

impl Plugin for ToolsPlugin {
    fn name(&self) -> &'static str {
        "tools"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        for tool in builtin_tools() {
            context.add_tool(tool)?;
        }
        Ok(())
    }
}

/// Build a registry holding the seven built-in tools.
pub fn default_registry() -> Result<ToolRegistry, ToolRegistryError> {
    let mut registry = ToolRegistry::new();
    for tool in builtin_tools() {
        registry.insert(tool)?;
    }
    Ok(registry)
}

fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(shell::BashTool),
        Arc::new(editing::EditFileTool),
        Arc::new(searching::GrepTool),
        Arc::new(listing::ListDirTool),
        Arc::new(reading::ReadFileTool),
        Arc::new(http::WebFetchTool),
        Arc::new(writing::WriteFileTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_core::{
        Config, EventRecord, LlmError, LlmErrorCode, LlmProvider, LlmRequest, LlmStream, Plugin,
        PluginContext, PluginError, SessionEvent, SessionHeader, SessionStore, SessionSummary,
        StoreError, SystemEnv, compose, ids::SessionId,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn expected_names() -> Vec<String> {
        [
            "bash",
            "edit_file",
            "grep",
            "list_dir",
            "read_file",
            "web_fetch",
            "write_file",
        ]
        .map(str::to_string)
        .to_vec()
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

    #[test]
    fn plugin_registers_every_builtin_tool() {
        assert_eq!(ToolsPlugin.name(), "tools");
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(ToolsPlugin),
        ];
        let runtime = compose(&plugins, &Config::default(), &SystemEnv).expect("compose");
        assert_eq!(runtime.tools.names(), expected_names());
    }

    #[test]
    fn default_registry_holds_every_builtin_tool() {
        let registry = default_registry().expect("registry");
        assert_eq!(registry.len(), 7);
        assert_eq!(registry.names(), expected_names());
    }

    #[test]
    fn default_registries_can_be_built_repeatedly() {
        assert!(default_registry().is_ok());
        assert!(default_registry().is_ok());
    }

    #[test]
    fn every_schema_is_a_json_object_with_a_description() {
        let registry = default_registry().expect("registry");
        for schema in registry.schemas() {
            assert!(schema.parameters.is_object(), "{}", schema.name);
            assert!(!schema.description.is_empty(), "{}", schema.name);
            assert_eq!(schema.parameters["type"], "object", "{}", schema.name);
        }
    }
}
