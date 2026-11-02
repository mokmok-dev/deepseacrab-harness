//! Fresh-agent rounds toward one immutable objective.

use crate::{AgentConfig, AgentLoop, RunOutcome};
use async_trait::async_trait;
use dsh_core::{
    Clock, LlmProvider, SessionId, SessionStore, Tool, ToolContext, ToolOutcome, ToolRegistry,
    ToolSchema,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Run bounded fresh-agent rounds toward one objective.
pub struct RalphTool {
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AgentConfig,
    clock: Arc<dyn Clock>,
}

impl RalphTool {
    /// Build a Ralph tool from the parent's seams.
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
struct RalphArgs {
    objective: String,
    #[serde(default = "default_rounds")]
    rounds: u32,
}

const fn default_rounds() -> u32 {
    3
}

struct Round {
    session_id: SessionId,
    outcome: RunOutcome,
}

#[async_trait]
impl Tool for RalphTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ralph".to_string(),
            description:
                "Run bounded fresh-agent rounds toward one immutable objective, carrying a handoff between rounds."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "objective": {"type": "string"},
                    "rounds": {"type": "integer", "minimum": 1, "maximum": 10}
                },
                "required": ["objective"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: RalphArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if args.objective.trim().is_empty() {
            return ToolOutcome::error("ralph objective must not be empty");
        }
        if args.rounds == 0 || args.rounds > 10 {
            return ToolOutcome::error("ralph rounds must be between 1 and 10");
        }
        let Some(parent) = context.session() else {
            return ToolOutcome::error("this tool requires a durable session");
        };
        let mut rounds = Vec::new();
        let mut handoff = String::from("(first round)");
        for _ in 0..args.rounds {
            let child_id = SessionId::generate();
            let mut config = self.config.clone();
            config.parent_session = Some(parent.session_id().clone());
            config.cwd = context.cwd.clone();
            config.system_prompt = format!(
                "{}\n\nYou are one fresh round in a Ralph loop. Work toward the immutable objective and return a concise handoff with status, summary, evidence, next steps, and blockers.",
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
            let task = format!(
                "Objective:\n{}\n\nPrevious handoff:\n{}\n\nContinue toward the objective.",
                args.objective, handoff
            );
            let outcome = match agent
                .run(&child_id, &task, context.cancel.clone(), None)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => return ToolOutcome::error(error.to_string()),
            };
            let failed = !outcome.is_success();
            handoff = if outcome.text.is_empty() {
                format!("round ended with {:?}", outcome.outcome)
            } else {
                outcome.text.clone()
            };
            rounds.push(Round {
                session_id: child_id,
                outcome,
            });
            if failed {
                break;
            }
        }
        ToolOutcome::text(render_rounds(&rounds))
    }
}

fn render_rounds(rounds: &[Round]) -> String {
    let completed = rounds
        .iter()
        .filter(|round| round.outcome.is_success())
        .count();
    let mut output = format!("ralph completed {completed}/{} rounds\n", rounds.len());
    for (index, round) in rounds.iter().enumerate() {
        output.push_str(&format!(
            "{}. {} {}\n{}\n",
            index + 1,
            round.session_id,
            if round.outcome.is_success() {
                "completed"
            } else {
                "failed"
            },
            round.outcome.text
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rounds_is_three() {
        assert_eq!(default_rounds(), 3);
    }

    #[test]
    fn rendering_counts_rounds() {
        let session_id = SessionId::generate();
        let completed = RunOutcome {
            session_id: session_id.clone(),
            outcome: dsh_core::TurnOutcome::Completed,
            text: "done".to_string(),
            usage: dsh_core::TokenUsage::default(),
            steps: 1,
        };
        let rendered = render_rounds(&[Round {
            session_id,
            outcome: completed,
        }]);
        assert!(rendered.starts_with("ralph completed 1/1 rounds\n"));
        assert!(rendered.contains("done"));
    }
}
