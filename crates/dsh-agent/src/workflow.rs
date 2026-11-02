//! Run an ordered workflow of child sessions.

use crate::{AgentConfig, AgentLoop, RunOutcome};
use async_trait::async_trait;
use dsh_core::{
    Clock, LlmProvider, SessionId, SessionStore, Tool, ToolContext, ToolOutcome, ToolRegistry,
    ToolSchema,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Run an ordered sequence of delegated tasks.
pub struct WorkflowTool {
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AgentConfig,
    clock: Arc<dyn Clock>,
}

impl WorkflowTool {
    /// Build a workflow tool from the parent's seams.
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
struct WorkflowArgs {
    steps: Vec<WorkflowStep>,
    #[serde(default = "default_stop_on_failure")]
    stop_on_failure: bool,
}

#[derive(Deserialize)]
struct WorkflowStep {
    task: String,
    #[serde(default)]
    session_id: Option<String>,
}

const fn default_stop_on_failure() -> bool {
    true
}

struct StepResult {
    session_id: SessionId,
    outcome: RunOutcome,
}

#[async_trait]
impl Tool for WorkflowTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "workflow".to_string(),
            description: "Run an ordered sequence of self-contained tasks in fresh child sessions."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "task": {"type": "string"},
                                "session_id": {"type": "string"}
                            },
                            "required": ["task"]
                        }
                    },
                    "stop_on_failure": {"type": "boolean"}
                },
                "required": ["steps"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: WorkflowArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if args.steps.is_empty() || args.steps.len() > 20 {
            return ToolOutcome::error("workflow needs between 1 and 20 steps");
        }
        if args.steps.iter().any(|step| step.task.trim().is_empty()) {
            return ToolOutcome::error("workflow step tasks must not be empty");
        }
        let Some(parent) = context.session() else {
            return ToolOutcome::error("this tool requires a durable session");
        };
        let mut results = Vec::new();
        for step in args.steps {
            let child_id = match step.session_id {
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
                "{}\n\nYou are one step in a workflow. Complete the assigned task and return a concise result.",
                config.system_prompt
            );
            let mut tools = self.tools.clone();
            tools.remove("subagent");
            tools.remove("workflow");
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
                .run(&child_id, &step.task, context.cancel.clone(), None)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => return ToolOutcome::error(error.to_string()),
            };
            let failed = !outcome.is_success();
            results.push(StepResult {
                session_id: child_id,
                outcome,
            });
            if failed && args.stop_on_failure {
                break;
            }
        }
        ToolOutcome::text(render_workflow(&results))
    }
}

fn render_workflow(results: &[StepResult]) -> String {
    let completed = results
        .iter()
        .filter(|result| result.outcome.is_success())
        .count();
    let mut output = format!("workflow completed {completed}/{} steps\n", results.len());
    for (index, result) in results.iter().enumerate() {
        let status = if result.outcome.is_success() {
            "completed"
        } else {
            "failed"
        };
        output.push_str(&format!(
            "{}. {} {}\n{}",
            index + 1,
            result.session_id,
            status,
            result.outcome.text
        ));
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_counts_and_labels_steps() {
        let success = RunOutcome {
            session_id: SessionId::generate(),
            outcome: dsh_core::TurnOutcome::Completed,
            text: "done".to_string(),
            usage: dsh_core::TokenUsage::default(),
            steps: 1,
        };
        let failure = RunOutcome {
            session_id: SessionId::generate(),
            outcome: dsh_core::TurnOutcome::StepLimit,
            text: "stopped".to_string(),
            usage: dsh_core::TokenUsage::default(),
            steps: 2,
        };
        let rendered = render_workflow(&[
            StepResult {
                session_id: success.session_id.clone(),
                outcome: success,
            },
            StepResult {
                session_id: failure.session_id.clone(),
                outcome: failure,
            },
        ]);
        assert!(rendered.starts_with("workflow completed 1/2 steps\n"));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("failed"));
        assert!(rendered.contains("done"));
        assert!(rendered.contains("stopped"));
    }
}
