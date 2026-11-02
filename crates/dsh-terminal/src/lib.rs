#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Persistent PTY terminals exposed to the model as four tools.
//!
//! A terminal is a spawned command plus its writer and a bounded output
//! buffer. The manager is process-local, so terminals live for as long as the
//! tool set does rather than being recorded in the session store.

mod id;
mod manager;
mod output;
mod tools;

use dsh_core::{Plugin, PluginContext, PluginError, Tool};
use std::sync::Arc;

/// Registers the terminal tools with the composed runtime.
pub struct TerminalPlugin;

impl Plugin for TerminalPlugin {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        for tool in default_terminal_tools() {
            context.add_tool(tool)?;
        }
        Ok(())
    }
}

/// Build the four terminal tools over one shared manager.
#[must_use]
pub fn default_terminal_tools() -> Vec<Arc<dyn Tool>> {
    tools::new_tool_set()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_core::{
        Config, EventRecord, LlmError, LlmErrorCode, LlmProvider, LlmRequest, LlmStream,
        SessionEvent, SessionHeader, SessionStore, SessionSummary, StoreError, SystemEnv, compose,
        ids::SessionId,
    };
    use tokio_util::sync::CancellationToken;

    fn expected_names() -> Vec<String> {
        [
            "terminal_close",
            "terminal_open",
            "terminal_read",
            "terminal_write",
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
    fn plugin_registers_every_terminal_tool() {
        assert_eq!(TerminalPlugin.name(), "terminal");
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(TerminalPlugin),
        ];
        let runtime = compose(&plugins, &Config::default(), &SystemEnv).expect("compose");
        assert_eq!(runtime.tools.names(), expected_names());
    }

    #[test]
    fn default_tools_cover_every_terminal_call() {
        let names: Vec<String> = default_terminal_tools()
            .iter()
            .map(|tool| tool.schema().name)
            .collect();
        let mut sorted = names;
        sorted.sort();
        assert_eq!(sorted, expected_names());
    }

    #[test]
    fn every_schema_is_a_json_object_with_a_description() {
        for tool in default_terminal_tools() {
            let schema = tool.schema();
            assert!(schema.parameters.is_object(), "{}", schema.name);
            assert_eq!(schema.parameters["type"], "object", "{}", schema.name);
            assert!(!schema.description.is_empty(), "{}", schema.name);
        }
    }
}
