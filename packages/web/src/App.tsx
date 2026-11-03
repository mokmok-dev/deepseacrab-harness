import { useCallback, useEffect, useState } from 'react';
import {
  createSession,
  errorMessage,
  fetchProjects,
  fetchSessions,
  type ProjectView,
  type SessionSummary,
} from './api';
import { NewSessionScreen } from './components/NewSessionScreen';
import { SessionView } from './components/SessionView';
import { Sidebar } from './components/Sidebar';
import { readSessionId, showNewSession } from './session';

type SessionList = {
  items: SessionSummary[];
  error: string | null;
};

type ProjectList = {
  items: ProjectView[];
  error: string | null;
  loaded: boolean;
};

type AppProps = {
  initialSessionId: string | null;
};

export function App({ initialSessionId }: AppProps) {
  const [activeSessionId, setActiveSessionId] = useState<string | null>(initialSessionId);
  const [sessionList, setSessionList] = useState<SessionList>({ items: [], error: null });
  const [projects, setProjects] = useState<ProjectList>({ items: [], error: null, loaded: false });
  const [drawerOpen, setDrawerOpen] = useState(false);

  const refreshSessions = useCallback(async () => {
    try {
      const items = await fetchSessions();
      setSessionList({ items, error: null });
    } catch (cause) {
      setSessionList((previous) => ({ ...previous, error: errorMessage(cause) }));
    }
  }, []);

  const refreshProjects = useCallback(async () => {
    try {
      const items = await fetchProjects();
      setProjects({ items, error: null, loaded: true });
    } catch (cause) {
      setProjects((previous) => ({ ...previous, error: errorMessage(cause), loaded: true }));
    }
  }, []);

  useEffect(() => {
    void refreshSessions();
  }, [refreshSessions]);

  useEffect(() => {
    void refreshProjects();
  }, [refreshProjects]);

  useEffect(() => {
    const handleHashChange = () => setActiveSessionId(readSessionId() || null);
    window.addEventListener('hashchange', handleHashChange);
    return () => window.removeEventListener('hashchange', handleHashChange);
  }, []);

  const openSession = (id: string) => {
    setDrawerOpen(false);
    if (id === activeSessionId) return;
    window.location.hash = id;
  };

  const startNewChat = () => {
    setDrawerOpen(false);
    setActiveSessionId(null);
    showNewSession();
    void refreshProjects();
  };

  const startSession = useCallback(
    async (path: string) => {
      const created = await createSession(path);
      setActiveSessionId(created.id);
      window.location.hash = created.id;
      await refreshSessions();
    },
    [refreshSessions],
  );

  return (
    <div className="app">
      <Sidebar
        sessions={sessionList.items}
        activeSessionId={activeSessionId}
        error={sessionList.error}
        open={drawerOpen}
        onSelect={openSession}
        onNewChat={startNewChat}
        onRefresh={() => void refreshSessions()}
        onClose={() => setDrawerOpen(false)}
      />
      {drawerOpen && (
        <div className="backdrop" aria-hidden="true" onClick={() => setDrawerOpen(false)} />
      )}
      {activeSessionId === null ? (
        <NewSessionScreen
          projects={projects.items}
          projectsLoaded={projects.loaded}
          projectsError={projects.error}
          sessions={sessionList.items}
          onRefreshProjects={refreshProjects}
          onStart={startSession}
          onOpenSessions={() => setDrawerOpen(true)}
        />
      ) : (
        <SessionView
          key={activeSessionId}
          sessionId={activeSessionId}
          onRunFinished={refreshSessions}
          onOpenSessions={() => setDrawerOpen(true)}
          onNewChat={startNewChat}
        />
      )}
    </div>
  );
}
