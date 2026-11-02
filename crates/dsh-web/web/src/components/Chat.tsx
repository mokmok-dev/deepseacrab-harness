import { useEffect, useRef, useState } from 'react';
import { fetchServerSentEvents, useChat, type UIMessage } from '@tanstack/ai-react';
import { Composer } from './Composer';
import { Transcript } from './Transcript';
import { markSessionStarted, shortSessionId } from '../session';

const connection = fetchServerSentEvents('/api/chat');

const EXAMPLES = [
  'Summarize this repository and list the main crates.',
  'Run the test suite and fix what fails.',
  'Find where the session log is written and explain the format.',
];

type ChatProps = {
  sessionId: string;
  initialMessages: UIMessage[];
  loadError: string | null;
  onRunFinished: () => void;
  onOpenSessions: () => void;
};

type ChatStatus = ReturnType<typeof useChat>['status'];

export function Chat({
  sessionId,
  initialMessages,
  loadError,
  onRunFinished,
  onOpenSessions,
}: ChatProps) {
  const { messages, sendMessage, isLoading, stop, error, status } = useChat({
    connection,
    threadId: sessionId,
    initialMessages,
  });
  const [draft, setDraft] = useState('');
  const transcriptRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const stickToBottom = useRef(true);
  const wasBusy = useRef(false);

  useEffect(() => {
    const node = transcriptRef.current;
    if (node !== null && stickToBottom.current) node.scrollTop = node.scrollHeight;
  }, [messages]);

  useEffect(() => {
    const busy = status === 'submitted' || status === 'streaming';
    if (wasBusy.current && !busy) onRunFinished();
    wasBusy.current = busy;
  }, [status, onRunFinished]);

  const handleScroll = () => {
    const node = transcriptRef.current;
    if (node === null) return;
    stickToBottom.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
  };

  const send = () => {
    const text = draft.trim();
    if (text === '' || isLoading) return;
    markSessionStarted(sessionId);
    setDraft('');
    void sendMessage(text);
  };

  const fillExample = (prompt: string) => {
    setDraft(prompt);
    inputRef.current?.focus();
  };

  const label = activityLabel(status, messages);
  const tone = status === 'error' ? 'error' : isLoading ? 'busy' : 'idle';
  const empty = messages.length === 0 && !isLoading;

  return (
    <main className="chat">
      <header className="topbar">
        <button
          type="button"
          className="icon-button menu-button"
          onClick={onOpenSessions}
          aria-label="Open sessions"
        >
          Sessions
        </button>
        <div className="topbar-title">
          <span className="topbar-name">DeepSeaCrab Harness</span>
          <span className="topbar-session" title={sessionId}>
            {shortSessionId(sessionId)}
          </span>
        </div>
        <span className={`status status-${tone}`} role="status">
          {label}
        </span>
      </header>

      {error !== undefined && (
        <div className="banner banner-error" role="alert">
          {error.message}
        </div>
      )}
      {loadError !== null && (
        <div className="banner banner-error" role="alert">
          Could not load this session log. {loadError}
        </div>
      )}

      <div
        className="transcript"
        ref={transcriptRef}
        onScroll={handleScroll}
        aria-live="polite"
      >
        {empty ? (
          <div className="empty">
            <h1>What should the harness do?</h1>
            <p>Each session runs in the workspace served by this process.</p>
            <div className="examples">
              {EXAMPLES.map((prompt) => (
                <button
                  key={prompt}
                  type="button"
                  className="example"
                  onClick={() => fillExample(prompt)}
                >
                  {prompt}
                </button>
              ))}
            </div>
          </div>
        ) : (
          <Transcript messages={messages} />
        )}
      </div>

      <Composer
        value={draft}
        onChange={setDraft}
        onSend={send}
        onStop={stop}
        loading={isLoading}
        inputRef={inputRef}
      />
    </main>
  );
}

function activityLabel(status: ChatStatus, messages: readonly UIMessage[]): string {
  switch (status) {
    case 'submitted':
      return 'running';
    case 'streaming':
      return streamingLabel(messages);
    case 'error':
      return 'error';
    case 'ready':
      return 'idle';
  }
}

function streamingLabel(messages: readonly UIMessage[]): string {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message === undefined || message.role !== 'assistant') continue;
    const part = message.parts[message.parts.length - 1];
    if (part === undefined) return 'running';
    switch (part.type) {
      case 'thinking':
        return 'thinking';
      case 'text':
        return 'writing';
      case 'tool-call':
        return `running ${part.name}`;
      case 'tool-result':
        return 'running tool';
      default:
        return 'running';
    }
  }
  return 'running';
}
