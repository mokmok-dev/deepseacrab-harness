import { useCallback, useEffect, useState } from 'react';
import { errorMessage, fetchSessions, type SessionSummary } from './api';
import { SessionView } from './components/SessionView';
import { Sidebar } from './components/Sidebar';
import { readSessionId, startNewSession } from './session';

type SessionList = {
  items: SessionSummary[];
  error: string | null;
};

type AppProps = {
  initialSessionId: string;
};

export function App({ initialSessionId }: AppProps) {
  const [activeSessionId, setActiveSessionId] = useState(initialSessionId);
  const [sessionList, setSessionList] = useState<SessionList>({ items: [], error: null });
  const [drawerOpen, setDrawerOpen] = useState(false);

  const refreshSessions = useCallback(async () => {
    try {
      const items = await fetchSessions();
      setSessionList({ items, error: null });
    } catch (cause) {
      setSessionList((previous) => ({ ...previous, error: errorMessage(cause) }));
    }
  }, []);

  useEffect(() => {
    void refreshSessions();
  }, [refreshSessions]);

  useEffect(() => {
    const handleHashChange = () => setActiveSessionId(readSessionId());
    window.addEventListener('hashchange', handleHashChange);
    return () => window.removeEventListener('hashchange', handleHashChange);
  }, []);

  const openSession = (id: string) => {
    setDrawerOpen(false);
    if (id === activeSessionId) return;
    window.location.hash = id;
  };

  const createSession = () => {
    setDrawerOpen(false);
    startNewSession();
  };

  return (
    <div className="app">
      <Sidebar
        sessions={sessionList.items}
        activeSessionId={activeSessionId}
        error={sessionList.error}
        open={drawerOpen}
        onSelect={openSession}
        onNewChat={createSession}
        onRefresh={() => void refreshSessions()}
        onClose={() => setDrawerOpen(false)}
      />
      {drawerOpen && (
        <div className="backdrop" aria-hidden="true" onClick={() => setDrawerOpen(false)} />
      )}
      <SessionView
        key={activeSessionId}
        sessionId={activeSessionId}
        onRunFinished={refreshSessions}
        onOpenSessions={() => setDrawerOpen(true)}
      />
    </div>
  );
}
