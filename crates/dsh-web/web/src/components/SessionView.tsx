import { useEffect, useState } from 'react';
import type { UIMessage } from '@tanstack/ai-react';
import { errorMessage, fetchSessionRecords } from '../api';
import { toUIMessages } from '../replay';
import { isNewSession } from '../session';
import { Chat } from './Chat';

type SessionViewProps = {
  sessionId: string;
  onRunFinished: () => void;
  onOpenSessions: () => void;
};

type Loaded = {
  messages: UIMessage[];
  error: string | null;
};

export function SessionView({ sessionId, onRunFinished, onOpenSessions }: SessionViewProps) {
  const [loaded, setLoaded] = useState<Loaded | null>(
    isNewSession(sessionId) ? { messages: [], error: null } : null,
  );

  useEffect(() => {
    if (isNewSession(sessionId)) return;

    let cancelled = false;
    fetchSessionRecords(sessionId)
      .then((records) => {
        if (cancelled) return;
        setLoaded({ messages: toUIMessages(records ?? []), error: null });
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setLoaded({ messages: [], error: errorMessage(cause) });
      });

    return () => {
      cancelled = true;
    };
  }, [sessionId]);

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
      onRunFinished={onRunFinished}
      onOpenSessions={onOpenSessions}
    />
  );
}
