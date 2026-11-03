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

export type ProjectView = {
  name: string;
  path: string;
  exists: boolean;
  builtin: boolean;
};

export async function fetchSessions(): Promise<SessionSummary[]> {
  const response = await fetch('/api/sessions');
  if (!response.ok) throw await responseError(response, 'Could not load sessions');
  return parseSessionSummaries(await response.json());
}

export async function fetchSessionRecords(id: string): Promise<EventRecord[] | null> {
  const response = await fetch(`/api/sessions/${encodeURIComponent(id)}`);
  if (response.status === 404) return null;
  if (!response.ok) throw await responseError(response, 'Could not load the session log');
  return parseRecords(await response.json());
}

export async function fetchProjects(): Promise<ProjectView[]> {
  const response = await fetch('/api/projects');
  if (!response.ok) throw await responseError(response, 'Could not load projects');
  return parseProjects(await response.json());
}

export async function addProject(path: string): Promise<ProjectView> {
  const response = await fetch('/api/projects', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ path }),
  });
  if (!response.ok) throw await responseError(response, 'Could not add the project');
  const project = parseProject(await response.json());
  if (project === null) throw new Error('The server did not return the project.');
  return project;
}

export async function removeProject(path: string): Promise<void> {
  const response = await fetch('/api/projects', {
    method: 'DELETE',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ path }),
  });
  if (!response.ok) throw await responseError(response, 'Could not remove the project');
}

export async function createSession(project: string | null): Promise<SessionSummary> {
  const response = await fetch('/api/sessions', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ project }),
  });
  if (!response.ok) throw await responseError(response, 'Could not start a session');
  const summary = parseSessionSummary(await response.json());
  if (summary === null) throw new Error('The server did not return the new session.');
  return summary;
}

export function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

async function responseError(response: Response, fallback: string): Promise<Error> {
  try {
    const payload: unknown = await response.json();
    if (isObject(payload)) {
      const message = stringField(payload, 'error');
      if (message !== null && message !== '') return new Error(message);
    }
  } catch {
    // The body carried no JSON to read.
  }
  return new Error(`${fallback} (${response.status}).`);
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
    const summary = parseSessionSummary(entry);
    if (summary === null) {
      console.warn('Skipping an unreadable session summary.', entry);
      return [];
    }
    return [summary];
  });
}

function parseSessionSummary(value: unknown): SessionSummary | null {
  if (!isObject(value)) return null;
  const id = stringField(value, 'id');
  const createdAt = numberField(value, 'created_at');
  const eventCount = numberField(value, 'event_count');
  if (id === null || createdAt === null || eventCount === null) return null;
  return {
    id,
    created_at: createdAt,
    cwd: stringField(value, 'cwd'),
    title: stringField(value, 'title'),
    event_count: eventCount,
  };
}

function parseProjects(value: unknown): ProjectView[] {
  if (!Array.isArray(value)) throw new Error('The project list was not an array.');
  return value.flatMap((entry) => {
    const project = parseProject(entry);
    if (project === null) {
      console.warn('Skipping an unreadable project.', entry);
      return [];
    }
    return [project];
  });
}

function parseProject(value: unknown): ProjectView | null {
  if (!isObject(value)) return null;
  const name = stringField(value, 'name');
  const path = stringField(value, 'path');
  const exists = value.exists;
  const builtin = value.builtin;
  if (name === null || path === null) return null;
  if (typeof exists !== 'boolean' || typeof builtin !== 'boolean') return null;
  return { name, path, exists, builtin };
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
    case 'working_directory_changed': {
      const cwd = stringField(value, 'cwd');
      return { type, cwd };
    }
    case 'turn_started':
    case 'step_started':
    case 'todo_written':
    case 'plan_set':
    case 'goal_set':
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
