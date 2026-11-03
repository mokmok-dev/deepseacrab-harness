import { useCallback, useEffect, useState } from 'react';
import type { UIMessage } from '@tanstack/ai-react';
import { errorMessage, fetchSessionRecords } from '../api';
import { latestWorkspace, toUIMessages } from '../replay';
import { Chat } from './Chat';

type SessionViewProps = {
  sessionId: string;
  onRunFinished: () => void;
  onOpenSessions: () => void;
  onNewChat: () => void;
};

type Loaded = {
  messages: UIMessage[];
  error: string | null;
};

export function SessionView({
  sessionId,
  onRunFinished,
  onOpenSessions,
  onNewChat,
}: SessionViewProps) {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [missing, setMissing] = useState(false);
  const [workspace, setWorkspace] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchSessionRecords(sessionId)
      .then((records) => {
        if (cancelled) return;
        if (records === null) {
          setMissing(true);
          return;
        }
        setWorkspace(latestWorkspace(records));
        setLoaded({ messages: toUIMessages(records), error: null });
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setLoaded({ messages: [], error: errorMessage(cause) });
      });

    return () => {
      cancelled = true;
    };
  }, [sessionId]);

  const refreshWorkspace = useCallback(async () => {
    try {
      const records = await fetchSessionRecords(sessionId);
      if (records !== null) setWorkspace(latestWorkspace(records));
    } catch {
      // The header keeps the workspace it last showed.
    }
  }, [sessionId]);

  const handleRunFinished = useCallback(() => {
    onRunFinished();
    void refreshWorkspace();
  }, [onRunFinished, refreshWorkspace]);

  if (missing) {
    return (
      <main className="chat">
        <div className="panel">
          <button
            type="button"
            className="icon-button menu-button"
            onClick={onOpenSessions}
            aria-label="Open sessions"
          >
            Sessions
          </button>
          <h1>This session does not exist</h1>
          <p>The server holds no log for it. Start a new chat to pick a project.</p>
          <button type="button" className="panel-action" onClick={onNewChat}>
            Start a new chat
          </button>
        </div>
      </main>
    );
  }

  if (loaded === null) {
    return (
      <main className="chat">
        <p className="loading-panel">Loading session…</p>
      </main>
    );
  }

  return (
    <Chat
      key={sessionId}
      sessionId={sessionId}
      initialMessages={loaded.messages}
      loadError={loaded.error}
      workspace={workspace}
      onRunFinished={handleRunFinished}
      onOpenSessions={onOpenSessions}
    />
  );
}
