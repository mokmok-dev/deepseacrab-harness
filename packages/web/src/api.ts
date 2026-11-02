import type {
  ContentBlock,
  EventRecord,
  Message,
  SessionEvent,
  ToolMessage,
  TurnOutcome,
} from './replay';

export type SessionSummary = {
  id: string;
  created_at: number;
  cwd: string | null;
  title: string | null;
  event_count: number;
};

export async function fetchSessions(): Promise<SessionSummary[]> {
  const response = await fetch('/api/sessions');
  if (!response.ok) {
    throw new Error(`Could not load sessions (${response.status}).`);
  }
  return parseSessionSummaries(await response.json());
}

export async function fetchSessionRecords(id: string): Promise<EventRecord[] | null> {
  const response = await fetch(`/api/sessions/${encodeURIComponent(id)}`);
  if (response.status === 404) return null;
  if (!response.ok) {
    throw new Error(`Could not load the session log (${response.status}).`);
  }
  return parseRecords(await response.json());
}

export function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringField(source: Record<string, unknown>, key: string): string | null {
  const value = source[key];
  return typeof value === 'string' ? value : null;
}

function numberField(source: Record<string, unknown>, key: string): number | null {
  const value = source[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function parseSessionSummaries(value: unknown): SessionSummary[] {
  if (!Array.isArray(value)) throw new Error('The session list was not an array.');
  return value.flatMap((entry) => {
    if (!isObject(entry)) {
      console.warn('Skipping an unreadable session summary.', entry);
      return [];
    }
    const id = stringField(entry, 'id');
    const createdAt = numberField(entry, 'created_at');
    const eventCount = numberField(entry, 'event_count');
    if (id === null || createdAt === null || eventCount === null) {
      console.warn('Skipping an unreadable session summary.', entry);
      return [];
    }
    return [
      {
        id,
        created_at: createdAt,
        cwd: stringField(entry, 'cwd'),
        title: stringField(entry, 'title'),
        event_count: eventCount,
      },
    ];
  });
}

function parseRecords(value: unknown): EventRecord[] {
  if (!Array.isArray(value)) throw new Error('The session log was not an array.');
  return value.flatMap((entry) => {
    if (!isObject(entry)) {
      console.warn('Skipping an unreadable session record.', entry);
      return [];
    }
    const seq = numberField(entry, 'seq');
    const at = numberField(entry, 'at');
    const event = parseEvent(entry.event);
    if (seq === null || at === null || event === null) {
      console.warn('Skipping an unreadable session record.', entry);
      return [];
    }
    return [{ seq, at, event }];
  });
}

function parseEvent(value: unknown): SessionEvent | null {
  if (!isObject(value)) return null;
  const type = value.type;
  if (typeof type !== 'string') return null;

  switch (type) {
    case 'user_message': {
      const message = parseMessage(value.message);
      return message === null ? null : { type, message };
    }
    case 'assistant_message': {
      const message = parseMessage(value.message);
      return message === null ? null : { type, message };
    }
    case 'tool_result': {
      const message = parseToolMessage(value.message);
      return message === null ? null : { type, message };
    }
    case 'turn_ended': {
      const turnId = stringField(value, 'turn_id');
      const outcome = parseOutcome(value.outcome);
      return turnId === null || outcome === null ? null : { type, turn_id: turnId, outcome };
    }
    case 'compaction': {
      const summary = stringField(value, 'summary');
      const keepFrom = numberField(value, 'keep_from');
      return summary === null || keepFrom === null ? null : { type, summary, keep_from: keepFrom };
    }
    case 'session_created':
    case 'turn_started':
    case 'step_started':
    case 'todo_written':
    case 'plan_set':
    case 'goal_set':
    case 'working_directory_changed':
    case 'retry_attempt':
      return { type };
    default:
      return null;
  }
}

function parseMessage(value: unknown): Message | null {
  if (!isObject(value)) return null;
  const id = stringField(value, 'id');
  const content = parseContent(value.content);
  if (id === null || content === null) return null;
  return {
    id,
    content,
    provider: stringField(value, 'provider') ?? undefined,
    model: stringField(value, 'model') ?? undefined,
    usage: value.usage,
  };
}

function parseToolMessage(value: unknown): ToolMessage | null {
  if (!isObject(value)) return null;
  const id = stringField(value, 'id');
  const callId = stringField(value, 'call_id');
  const content = parseContent(value.content);
  const isError = value.is_error;
  if (id === null || callId === null || content === null || typeof isError !== 'boolean') {
    return null;
  }
  return { id, call_id: callId, content, is_error: isError };
}

function parseOutcome(value: unknown): TurnOutcome | null {
  if (!isObject(value)) return null;
  const outcome = value.outcome;
  if (
    outcome !== 'completed' &&
    outcome !== 'cancelled' &&
    outcome !== 'failed' &&
    outcome !== 'step_limit'
  ) {
    return null;
  }
  return {
    outcome,
    reason: stringField(value, 'reason') ?? undefined,
    code: stringField(value, 'code') ?? undefined,
    message: stringField(value, 'message') ?? undefined,
  };
}

function parseContent(value: unknown): ContentBlock[] | null {
  if (!Array.isArray(value)) return null;
  return value.flatMap((entry) => {
    const block = parseBlock(entry);
    return block === null ? [] : [block];
  });
}

function parseBlock(value: unknown): ContentBlock | null {
  if (!isObject(value)) return null;
  const type = value.type;
  if (typeof type !== 'string') return null;

  switch (type) {
    case 'text':
    case 'reasoning': {
      const text = stringField(value, 'text');
      return text === null ? null : { type, text };
    }
    case 'tool_call': {
      const call = value.call;
      if (!isObject(call)) return null;
      const id = stringField(call, 'id');
      const name = stringField(call, 'name');
      const args = stringField(call, 'arguments');
      if (id === null || name === null || args === null) return null;
      return { type, call: { id, name, arguments: args } };
    }
    case 'image': {
      const mediaType = stringField(value, 'media_type');
      const data = stringField(value, 'data');
      if (mediaType === null || data === null) return null;
      return { type, media_type: mediaType, data };
    }
    default:
      return null;
  }
}
