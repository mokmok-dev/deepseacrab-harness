#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Background job tools for the harness.
//!
//! A job is a `bash -lc` process started through [`JobStartTool`] and recorded
//! in `.dsh-jobs` under the session working directory: one file each for
//! stdout, stderr, pid, and the final status. The same directory is what the
//! listing, output, and kill tools read.

mod files;
mod kill;
mod list;
mod output;
mod start;
mod stream;

#[cfg(test)]
mod test_util;

use dsh_core::{Plugin, PluginContext, PluginError, Tool};
use kill::JobKillTool;
use list::JobListTool;
use output::JobOutputTool;
use start::JobStartTool;
use std::sync::Arc;

/// Registers the background job tools with the composed runtime.
pub struct JobsPlugin;

impl Plugin for JobsPlugin {
    fn name(&self) -> &'static str {
        "jobs"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        for tool in default_jobs() {
            context.add_tool(tool)?;
        }
        Ok(())
    }
}

/// Build the background job tools.
#[must_use]
pub fn default_jobs() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(JobStartTool),
        Arc::new(JobListTool),
        Arc::new(JobOutputTool),
        Arc::new(JobKillTool),
    ]
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
        ["job_kill", "job_list", "job_output", "job_start"]
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
    fn plugin_registers_every_job_tool() {
        assert_eq!(JobsPlugin.name(), "jobs");
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(StorePlugin),
            Box::new(ProviderPlugin),
            Box::new(JobsPlugin),
        ];
        let runtime = compose(&plugins, &Config::default(), &SystemEnv).expect("compose");
        assert_eq!(runtime.tools.names(), expected_names());
    }

    #[test]
    fn default_jobs_holds_every_job_tool() {
        let names: Vec<String> = default_jobs()
            .iter()
            .map(|tool| tool.schema().name)
            .collect();
        assert_eq!(names.len(), expected_names().len());
        let mut sorted = names;
        sorted.sort();
        assert_eq!(sorted, expected_names());
    }

    #[test]
    fn every_schema_is_a_json_object_with_a_description() {
        for tool in default_jobs() {
            let schema = tool.schema();
            assert!(schema.parameters.is_object(), "{}", schema.name);
            assert!(!schema.description.is_empty(), "{}", schema.name);
            assert_eq!(schema.parameters["type"], "object", "{}", schema.name);
        }
    }
}
