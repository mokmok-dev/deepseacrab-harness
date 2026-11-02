import type { UIMessage } from '@tanstack/ai-react';

type ToolCallPart = Extract<UIMessage['parts'][number], { type: 'tool-call' }>;
type ToolResultPart = Extract<UIMessage['parts'][number], { type: 'tool-result' }>;

export function ToolCallCard({ part }: { part: ToolCallPart }) {
  const output = formatOutput(part.output);

  return (
    <div className={part.state === 'error' ? 'tool-call tool-call-error' : 'tool-call'}>
      <div className="tool-call-head">
        <span className="tool-name">{part.name}</span>
        <span className="tool-state">{callStateLabel(part.state)}</span>
      </div>
      <details className="tool-args">
        <summary>Arguments</summary>
        <pre>{prettyArguments(part.arguments)}</pre>
      </details>
      {output !== null && (
        <div className="tool-output">
          <span className="tool-output-label">Output</span>
          <pre>{output}</pre>
        </div>
      )}
    </div>
  );
}

export function ToolResultCard({ part }: { part: ToolResultPart }) {
  const body = formatContent(part.content) || part.error || '(no output)';

  return (
    <div className={part.state === 'error' ? 'tool-call tool-call-error' : 'tool-call'}>
      <div className="tool-call-head">
        <span className="tool-name">{part.name ?? 'tool result'}</span>
        <span className="tool-state">{part.state}</span>
      </div>
      <div className="tool-output">
        <pre>{body}</pre>
      </div>
    </div>
  );
}

function prettyArguments(raw: string): string {
  if (raw.trim() === '') return '(no arguments)';
  try {
    return JSON.stringify(JSON.parse(raw), null, 2) ?? raw;
  } catch {
    return raw;
  }
}

function formatOutput(value: unknown): string | null {
  if (value === undefined) return null;
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function formatContent(content: ToolResultPart['content']): string {
  if (typeof content === 'string') return content;
  return content.flatMap((part) => (part.type === 'text' ? [part.content] : [])).join('');
}

function callStateLabel(state: ToolCallPart['state']): string {
  switch (state) {
    case 'awaiting-input':
      return 'awaiting input';
    case 'input-streaming':
      return 'streaming';
    case 'input-complete':
      return 'ready';
    case 'approval-requested':
      return 'approval';
    case 'approval-responded':
      return 'approved';
    case 'complete':
      return 'done';
    case 'error':
      return 'error';
  }
}
