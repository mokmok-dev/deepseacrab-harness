#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Durable scheduled follow-ups for the harness.
//!
//! A schedule is one recurring task, one interval, and one recorded session.
//! The list lives in a JSON file under the harness state directory, the model
//! edits it through three tools, and [`run_due`] turns every due entry into an
//! ordinary agent turn in that entry's session.

mod file;
mod runner;
mod tools;

use dsh_core::{Plugin, PluginContext, PluginError, Tool};
use std::sync::Arc;

pub use file::{MAX_INTERVAL_SECONDS, Schedule, ScheduleError, ScheduleFile};
pub use runner::{ScheduleRun, run_due};

/// Registers the schedule tools with the composed runtime.
pub struct SchedulePlugin;

impl Plugin for SchedulePlugin {
    fn name(&self) -> &'static str {
        "schedule"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        for tool in default_schedule_tools() {
            context.add_tool(tool)?;
        }
        Ok(())
    }
}

/// Build the schedule tools.
#[must_use]
pub fn default_schedule_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(tools::ScheduleCreateTool),
        Arc::new(tools::ScheduleListTool),
        Arc::new(tools::ScheduleCancelTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_core::{
        Config, EventRecord, LlmError, LlmProvider, LlmRequest, LlmStream, SessionEvent,
        SessionHeader, SessionStore, SessionSummary, SystemEnv, compose, ids::SessionId,
    };
    use tokio_util::sync::CancellationToken;

    struct NoStore;

    impl SessionStore for NoStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), dsh_core::StoreError> {
            Ok(())
        }

        fn append(
            &self,
            _session_id: &SessionId,
            _at: i64,
            _event: SessionEvent,
        ) -> Result<EventRecord, dsh_core::StoreError> {
            Err(dsh_core::StoreError::Backend("unused".to_string()))
        }

        fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, dsh_core::StoreError> {
            Ok(Vec::new())
        }

        fn list(&self) -> Result<Vec<SessionSummary>, dsh_core::StoreError> {
            Ok(Vec::new())
        }

        fn has(&self, _session_id: &SessionId) -> Result<bool, dsh_core::StoreError> {
            Ok(false)
        }

        fn delete(&self, _session_id: &SessionId) -> Result<bool, dsh_core::StoreError> {
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
            Err(LlmError::missing_credential("unused"))
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
    fn the_plugin_registers_three_schedule_tools() {
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(SchedulePlugin),
        ];
        let runtime = compose(&plugins, &Config::default(), &SystemEnv).expect("compose");
        assert_eq!(
            runtime.tools.names(),
            vec![
                "schedule_cancel".to_string(),
                "schedule_create".to_string(),
                "schedule_list".to_string(),
            ]
        );
    }

    #[test]
    fn the_plugin_reports_its_name() {
        assert_eq!(SchedulePlugin.name(), "schedule");
    }

    #[test]
    fn the_default_tools_are_the_three_schedule_tools() {
        let names: Vec<String> = default_schedule_tools()
            .iter()
            .map(|tool| tool.schema().name)
            .collect();
        assert_eq!(
            names,
            vec![
                "schedule_create".to_string(),
                "schedule_list".to_string(),
                "schedule_cancel".to_string(),
            ]
        );
    }
}
