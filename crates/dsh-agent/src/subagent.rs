//! Delegate a task to a child session.

use crate::{AgentConfig, AgentLoop};
use async_trait::async_trait;
use dsh_core::{
    Clock, LlmProvider, SessionId, SessionStore, Tool, ToolContext, ToolOutcome, ToolRegistry,
    ToolSchema,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Run a delegated task in a fresh child session.
pub struct SubagentTool {
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AgentConfig,
    clock: Arc<dyn Clock>,
}

impl SubagentTool {
    /// Build a delegation tool from the parent's seams.
    #[must_use]
    pub fn new(
        store: Arc<dyn SessionStore>,
        provider: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        config: AgentConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            provider,
            tools,
            config,
            clock,
        }
    }
}

#[derive(Deserialize)]
struct SubagentArgs {
    task: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[async_trait]
impl Tool for SubagentTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent".to_string(),
            description:
                "Delegate a self-contained task to a fresh child session and return its result."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "session_id": {"type": "string"}
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: SubagentArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if args.task.trim().is_empty() {
            return ToolOutcome::error("subagent task must not be empty");
        }
        let Some(parent) = context.session() else {
            return ToolOutcome::error("this tool requires a durable session");
        };
        let child_id = match args.session_id {
            Some(raw) => match SessionId::parse(raw) {
                Ok(id) => id,
                Err(error) => return ToolOutcome::error(error.to_string()),
            },
            None => SessionId::generate(),
        };
        let mut config = self.config.clone();
        config.parent_session = Some(parent.session_id().clone());
        config.cwd = context.cwd.clone();
        config.system_prompt = format!(
            "{}\n\nYou are a delegated subagent. Complete the assigned task and return a concise result.",
            config.system_prompt
        );
        let mut tools = self.tools.clone();
        tools.remove("subagent");
        tools.remove("workflow");
        tools.remove("ralph");
        let agent = match AgentLoop::new(
            Arc::clone(&self.store),
            Arc::clone(&self.provider),
            tools,
            config,
            Arc::clone(&self.clock),
        ) {
            Ok(agent) => agent,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let outcome = match agent
            .run(&child_id, &args.task, context.cancel.clone(), None)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        if outcome.is_success() {
            ToolOutcome::text(format!("subagent {child_id} completed:\n{}", outcome.text))
        } else {
            ToolOutcome::error(format!(
                "subagent {child_id} ended before completion:\n{}",
                outcome.text
            ))
        }
    }
}
