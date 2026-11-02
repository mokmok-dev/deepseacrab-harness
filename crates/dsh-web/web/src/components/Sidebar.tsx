import type { SessionSummary } from '../api';
import { shortSessionId } from '../session';

type SidebarProps = {
  sessions: SessionSummary[];
  activeSessionId: string;
  error: string | null;
  open: boolean;
  onSelect: (id: string) => void;
  onNewChat: () => void;
  onRefresh: () => void;
  onClose: () => void;
};

export function Sidebar({
  sessions,
  activeSessionId,
  error,
  open,
  onSelect,
  onNewChat,
  onRefresh,
  onClose,
}: SidebarProps) {
  return (
    <aside className={open ? 'sidebar sidebar-open' : 'sidebar'} aria-label="Sessions">
      <div className="sidebar-head">
        <span className="brand">DeepSeaCrab Harness</span>
        <button
          type="button"
          className="icon-button close-button"
          onClick={onClose}
          aria-label="Close sessions"
        >
          ×
        </button>
      </div>
      <button type="button" className="new-chat" onClick={onNewChat}>
        New chat
      </button>
      <div className="sidebar-row">
        <h2>Sessions</h2>
        <button
          type="button"
          className="icon-button"
          onClick={onRefresh}
          aria-label="Refresh sessions"
          title="Refresh sessions"
        >
          ↻
        </button>
      </div>
      {error !== null && (
        <p className="sidebar-error" role="alert">
          {error}
        </p>
      )}
      <ul className="session-list" aria-label="Session list">
        {sessions.length === 0 ? (
          <li className="session-empty">No sessions yet.</li>
        ) : (
          sessions.map((session) => (
            <li key={session.id}>
              <button
                type="button"
                className="session-button"
                aria-current={session.id === activeSessionId ? 'page' : undefined}
                onClick={() => onSelect(session.id)}
              >
                <span className="session-id">{shortSessionId(session.id)}</span>
                {session.title !== null && session.title !== '' && (
                  <span className="session-title">{session.title}</span>
                )}
                <span className="session-meta">
                  {formatCreatedAt(session.created_at)} · {session.event_count} events
                </span>
              </button>
            </li>
          ))
        )}
      </ul>
    </aside>
  );
}

function formatCreatedAt(millis: number): string {
  return new Date(millis).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}
