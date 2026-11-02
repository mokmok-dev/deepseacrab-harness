import type { UIMessage } from '@tanstack/ai-react';

export type ToolCall = {
  id: string;
  name: string;
  arguments: string;
};

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'reasoning'; text: string }
  | { type: 'tool_call'; call: ToolCall }
  | { type: 'image'; media_type: string; data: string };

export type Message = {
  id: string;
  content: ContentBlock[];
  provider?: string;
  model?: string;
  usage?: unknown;
};

export type ToolMessage = {
  id: string;
  call_id: string;
  content: ContentBlock[];
  is_error: boolean;
};

export type TurnOutcome = {
  outcome: 'completed' | 'cancelled' | 'failed' | 'step_limit';
  reason?: string;
  code?: string;
  message?: string;
};

export type Compaction = {
  summary: string;
  keep_from: number;
};

type ManagementEventType =
  | 'turn_started'
  | 'step_started'
  | 'todo_written'
  | 'plan_set'
  | 'goal_set'
  | 'retry_attempt';

export type SessionEvent =
  | { type: 'user_message'; message: Message }
  | { type: 'assistant_message'; message: Message }
  | { type: 'tool_result'; message: ToolMessage }
  | { type: 'turn_ended'; turn_id: string; outcome: TurnOutcome }
  | ({ type: 'compaction' } & Compaction)
  | { type: 'session_created'; cwd: string | null }
  | { type: 'working_directory_changed'; cwd: string | null }
  | { type: ManagementEventType };

export type EventRecord = {
  seq: number;
  at: number;
  event: SessionEvent;
};

/**
 * The directory a session's tools run in, read the same way the server reads it.
 * An empty value is skipped because an empty path is a prefix of every path.
 */
export function latestWorkspace(records: EventRecord[]): string | null {
  for (let index = records.length - 1; index >= 0; index -= 1) {
    const record = records[index];
    if (record === undefined) continue;
    const event = record.event;
    if (event.type === 'working_directory_changed' || event.type === 'session_created') {
      if (event.cwd !== null && event.cwd !== '') return event.cwd;
    }
  }
  return null;
}

type ToolCallPart = Extract<UIMessage['parts'][number], { type: 'tool-call' }>;

export function toUIMessages(records: EventRecord[]): UIMessage[] {
  const compaction = lastCompaction(records);
  const visible = compaction === undefined
    ? records
    : records.filter((record) => record.seq >= compaction.keep_from);

  const messages: UIMessage[] = [];
  const toolCalls = new Map<string, ToolCallPart>();

  if (compaction !== undefined) {
    messages.push({
      id: `compaction-${compaction.seq}`,
      role: 'system',
      parts: [
        {
          type: 'text',
          content: `Earlier conversation was summarized:\n${compaction.summary}`,
        },
      ],
    });
  }

  for (const { seq, event } of visible) {
    switch (event.type) {
      case 'user_message':
        messages.push({
          id: event.message.id,
          role: 'user',
          parts: textParts(event.message.content),
        });
        break;

      case 'assistant_message': {
        const parts: UIMessage['parts'] = [];
        for (const block of event.message.content) {
          const part = assistantPart(block);
          if (part === null) continue;
          parts.push(part);
          if (part.type === 'tool-call') toolCalls.set(part.id, part);
        }
        messages.push({ id: event.message.id, role: 'assistant', parts });
        break;
      }

      case 'tool_result': {
        const call = toolCalls.get(event.message.call_id);
        if (call === undefined) break;
        call.output = joinText(event.message.content);
        call.state = event.message.is_error ? 'error' : 'complete';
        break;
      }

      case 'turn_ended':
        if (event.outcome.outcome !== 'completed') {
          messages.push({
            id: `turn-${seq}`,
            role: 'system',
            parts: [{ type: 'text', content: outcomeText(event.outcome) }],
          });
        }
        break;

      default:
        break;
    }
  }

  return messages;
}

function lastCompaction(records: EventRecord[]): (Compaction & { seq: number }) | undefined {
  for (let index = records.length - 1; index >= 0; index -= 1) {
    const record = records[index];
    if (record !== undefined && record.event.type === 'compaction') {
      return { ...record.event, seq: record.seq };
    }
  }
  return undefined;
}

function textParts(blocks: ContentBlock[]): UIMessage['parts'] {
  return blocks.flatMap((block) =>
    block.type === 'text' ? [{ type: 'text' as const, content: block.text }] : [],
  );
}

function assistantPart(block: ContentBlock): UIMessage['parts'][number] | null {
  switch (block.type) {
    case 'text':
      return { type: 'text', content: block.text };
    case 'reasoning':
      return { type: 'thinking', content: block.text };
    case 'tool_call':
      return {
        type: 'tool-call',
        id: block.call.id,
        name: block.call.name,
        arguments: block.call.arguments,
        input: parseArguments(block.call.arguments),
        state: 'input-complete',
      };
    case 'image':
      return {
        type: 'image',
        source: { type: 'data', value: block.data, mimeType: block.media_type },
      };
  }
}

function parseArguments(raw: string): unknown {
  try {
    return JSON.parse(raw) as unknown;
  } catch {
    return undefined;
  }
}

function joinText(blocks: ContentBlock[]): string {
  return blocks.flatMap((block) => (block.type === 'text' ? [block.text] : [])).join('');
}

function outcomeText(outcome: TurnOutcome): string {
  const detail = outcome.message ?? outcome.reason ?? outcome.code;
  switch (outcome.outcome) {
    case 'cancelled':
      return detail === undefined ? 'Turn cancelled.' : `Turn cancelled: ${detail}`;
    case 'failed':
      return detail === undefined ? 'Turn failed.' : `Turn failed: ${detail}`;
    case 'step_limit':
      return detail === undefined
        ? 'Turn stopped at the step limit.'
        : `Turn stopped at the step limit: ${detail}`;
    case 'completed':
      return '';
  }
}
