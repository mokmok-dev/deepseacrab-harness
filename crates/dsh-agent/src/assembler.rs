//! Accumulate one streamed model response into a durable assistant message.

use crate::runner::AgentError;
use dsh_core::{
    AssistantMessage, ContentBlock, FinishReason, ModelId, ProviderId, StreamEvent, TokenUsage,
    ToolCall, ToolCallId,
};

/// One streamed response under construction.
#[derive(Debug, Default)]
pub struct AssistantAssembler {
    blocks: Vec<BlockBuilder>,
    usage: TokenUsage,
    finish: Option<FinishReason>,
}

#[derive(Debug)]
enum BlockBuilder {
    Text(String),
    Reasoning(String),
    Call(CallBuilder),
}

#[derive(Debug)]
struct CallBuilder {
    id: ToolCallId,
    name: String,
    arguments: String,
}

impl AssistantAssembler {
    /// Start an empty response.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit one provider event.
    pub fn push(&mut self, event: StreamEvent) -> Result<(), AgentError> {
        match event {
            StreamEvent::TextDelta { text } => match self.blocks.last_mut() {
                Some(BlockBuilder::Text(current)) => current.push_str(&text),
                _ => self.blocks.push(BlockBuilder::Text(text)),
            },
            StreamEvent::ReasoningDelta { text } => match self.blocks.last_mut() {
                Some(BlockBuilder::Reasoning(current)) => current.push_str(&text),
                _ => self.blocks.push(BlockBuilder::Reasoning(text)),
            },
            StreamEvent::ToolCallStart { id, name } => {
                if name.is_empty() {
                    return Err(AgentError::MalformedStream(
                        "tool call name is empty".to_string(),
                    ));
                }
                if self.calls().any(|call| call.id == id) {
                    return Err(AgentError::MalformedStream(format!(
                        "duplicate tool call id {id}"
                    )));
                }
                self.blocks.push(BlockBuilder::Call(CallBuilder {
                    id,
                    name,
                    arguments: String::new(),
                }));
            }
            StreamEvent::ToolCallDelta { id, arguments } => {
                let Some(call) = self.calls_mut().find(|call| call.id == id) else {
                    return Err(AgentError::MalformedStream(format!(
                        "tool call delta for unknown id {id}"
                    )));
                };
                call.arguments.push_str(&arguments);
            }
            StreamEvent::Usage { usage } => self.usage = usage,
            StreamEvent::Finish { reason } => {
                if self.finish.is_some() {
                    return Err(AgentError::MalformedStream(
                        "duplicate finish event".to_string(),
                    ));
                }
                self.finish = Some(reason);
            }
        }
        Ok(())
    }

    /// Settle the response.
    pub fn finish(
        self,
        provider: ProviderId,
        model: ModelId,
    ) -> Result<AssistantMessage, AgentError> {
        let reason = self.finish.ok_or(AgentError::MissingFinish)?;
        let mut content = Vec::new();
        let mut saw_call = false;
        for block in self.blocks {
            match block {
                BlockBuilder::Text(text) => {
                    if !text.is_empty() {
                        content.push(ContentBlock::text(text));
                    }
                }
                BlockBuilder::Reasoning(text) => {
                    if !text.is_empty() {
                        content.push(ContentBlock::reasoning(text));
                    }
                }
                BlockBuilder::Call(call) => {
                    saw_call = true;
                    let call = ToolCall::new(call.id, call.name, call.arguments)
                        .map_err(|error| AgentError::MalformedStream(error.to_string()))?;
                    content.push(ContentBlock::tool_call(call));
                }
            }
        }
        if content.is_empty() {
            return Err(AgentError::EmptyResponse);
        }
        if reason == FinishReason::ToolCalls && !saw_call {
            return Err(AgentError::MalformedStream(
                "tool-calls finish with no tool call".to_string(),
            ));
        }
        Ok(AssistantMessage::new(content, provider, model, self.usage))
    }

    fn calls(&self) -> impl Iterator<Item = &CallBuilder> {
        self.blocks.iter().filter_map(|block| match block {
            BlockBuilder::Call(call) => Some(call),
            BlockBuilder::Text(_) | BlockBuilder::Reasoning(_) => None,
        })
    }

    fn calls_mut(&mut self) -> impl Iterator<Item = &mut CallBuilder> {
        self.blocks.iter_mut().filter_map(|block| match block {
            BlockBuilder::Call(call) => Some(call),
            BlockBuilder::Text(_) | BlockBuilder::Reasoning(_) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ids() -> (ProviderId, ModelId) {
        (
            ProviderId::parse("deepseek").expect("provider"),
            ModelId::parse("deepseek-flash").expect("model"),
        )
    }

    fn text_stream() -> impl Strategy<Value = Vec<StreamEvent>> {
        prop::collection::vec("[a-zA-Z0-9 ]{1,12}", 1..8).prop_map(|chunks| {
            let mut events: Vec<_> = chunks
                .into_iter()
                .map(|text| StreamEvent::TextDelta { text })
                .collect();
            events.push(StreamEvent::Finish {
                reason: FinishReason::Stop,
            });
            events
        })
    }

    proptest! {
        #[test]
        fn text_chunks_concatenate_in_order(events in text_stream()) {
            let expected: String = events.iter().filter_map(|event| match event {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            }).collect();
            let mut assembler = AssistantAssembler::new();
            for event in events {
                assembler.push(event).expect("event");
            }
            let message = assembler.finish(ids().0, ids().1).expect("finish");
            prop_assert_eq!(message.text(), expected);
        }
    }

    #[test]
    fn blocks_keep_provider_order() {
        let (provider, model) = ids();
        let id = ToolCallId::parse("call-1").expect("id");
        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::TextDelta {
                text: "before".to_string(),
            })
            .expect("text");
        assembler
            .push(StreamEvent::ToolCallStart {
                id: id.clone(),
                name: "bash".to_string(),
            })
            .expect("start");
        assembler
            .push(StreamEvent::ToolCallDelta {
                id,
                arguments: r#"{"command":"true"}"#.to_string(),
            })
            .expect("delta");
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::ToolCalls,
            })
            .expect("finish");
        let message = assembler.finish(provider, model).expect("message");
        assert_eq!(message.content.len(), 2);
        assert_eq!(message.content[0], ContentBlock::text("before"));
        assert!(
            matches!(&message.content[1], ContentBlock::ToolCall { call } if call.name == "bash")
        );
    }

    #[test]
    fn a_delta_replaces_the_initial_arguments() {
        let (provider, model) = ids();
        let id = ToolCallId::parse("call-1").expect("id");
        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::ToolCallStart {
                id: id.clone(),
                name: "echo".to_string(),
            })
            .expect("start");
        assembler
            .push(StreamEvent::ToolCallDelta {
                id,
                arguments: r#"{"a":1}"#.to_string(),
            })
            .expect("delta");
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::ToolCalls,
            })
            .expect("finish");
        let message = assembler.finish(provider, model).expect("message");
        let call = message.tool_calls()[0];
        assert_eq!(
            call.arguments_value().expect("object"),
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn consecutive_deltas_form_single_blocks() {
        let (provider, model) = ids();
        let mut assembler = AssistantAssembler::new();
        for text in ["a", "b", "c"] {
            assembler
                .push(StreamEvent::TextDelta {
                    text: text.to_string(),
                })
                .expect("text");
        }
        for text in ["x", "y"] {
            assembler
                .push(StreamEvent::ReasoningDelta {
                    text: text.to_string(),
                })
                .expect("reasoning");
        }
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .expect("finish");
        let message = assembler.finish(provider, model).expect("message");
        assert_eq!(message.content.len(), 2);
        assert_eq!(message.content[0], ContentBlock::text("abc"));
        assert_eq!(message.content[1], ContentBlock::reasoning("xy"));
    }

    #[test]
    fn reasoning_only_responses_are_kept() {
        let (provider, model) = ids();
        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::ReasoningDelta {
                text: "why".to_string(),
            })
            .expect("reasoning");
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .expect("finish");
        let message = assembler.finish(provider, model).expect("message");
        assert_eq!(message.content, vec![ContentBlock::reasoning("why")]);
    }

    #[test]
    fn empty_deltas_do_not_create_blocks() {
        let (provider, model) = ids();
        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::TextDelta {
                text: String::new(),
            })
            .expect("text");
        assembler
            .push(StreamEvent::ReasoningDelta {
                text: String::new(),
            })
            .expect("reasoning");
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .expect("finish");
        assert!(matches!(
            assembler.finish(provider, model),
            Err(AgentError::EmptyResponse)
        ));
    }

    #[test]
    fn duplicate_calls_and_finishes_are_rejected() {
        let id = ToolCallId::parse("call-1").expect("id");
        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::ToolCallStart {
                id: id.clone(),
                name: "echo".to_string(),
            })
            .expect("start");
        assert!(
            assembler
                .push(StreamEvent::ToolCallStart {
                    id,
                    name: "echo".to_string(),
                })
                .is_err()
        );

        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .expect("finish");
        assert!(
            assembler
                .push(StreamEvent::Finish {
                    reason: FinishReason::Stop,
                })
                .is_err()
        );
    }

    #[test]
    fn missing_finish_and_empty_content_are_errors() {
        let (provider, model) = ids();
        assert!(matches!(
            AssistantAssembler::new().finish(provider.clone(), model.clone()),
            Err(AgentError::MissingFinish)
        ));

        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .expect("finish");
        assert!(matches!(
            assembler.finish(provider, model),
            Err(AgentError::EmptyResponse)
        ));
    }

    #[test]
    fn tool_finish_requires_a_call() {
        let (provider, model) = ids();
        let mut assembler = AssistantAssembler::new();
        assembler
            .push(StreamEvent::TextDelta {
                text: "text".to_string(),
            })
            .expect("text");
        assembler
            .push(StreamEvent::Finish {
                reason: FinishReason::ToolCalls,
            })
            .expect("finish");
        assert!(matches!(
            assembler.finish(provider, model),
            Err(AgentError::MalformedStream(_))
        ));
    }

    #[test]
    fn deltas_for_unknown_calls_are_rejected() {
        let mut assembler = AssistantAssembler::new();
        assert!(
            assembler
                .push(StreamEvent::ToolCallDelta {
                    id: ToolCallId::parse("missing").expect("id"),
                    arguments: "{}".to_string(),
                })
                .is_err()
        );
    }
}
