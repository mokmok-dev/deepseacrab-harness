import { useState } from 'react';
import {
  addProject,
  errorMessage,
  removeProject,
  type ProjectView,
  type SessionSummary,
} from '../api';

type NewSessionScreenProps = {
  projects: ProjectView[];
  projectsLoaded: boolean;
  projectsError: string | null;
  sessions: SessionSummary[];
  onRefreshProjects: () => Promise<void>;
  onStart: (path: string) => Promise<void>;
  onOpenSessions: () => void;
};

export function NewSessionScreen({
  projects,
  projectsLoaded,
  projectsError,
  sessions,
  onRefreshProjects,
  onStart,
  onOpenSessions,
}: NewSessionScreenProps) {
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const [adding, setAdding] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const workspacePath = projects.find((project) => project.builtin)?.path ?? null;
  const currentPath = selectedPath ?? defaultProjectPath(projects, sessions);
  const selected = projects.find((project) => project.path === currentPath) ?? null;
  const canStart = selected !== null && selected.exists && !creating;

  const add = async () => {
    const path = draft.trim();
    if (path === '' || adding) return;
    setAdding(true);
    setError(null);
    try {
      const project = await addProject(path);
      setDraft('');
      await onRefreshProjects();
      setSelectedPath(project.path);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setAdding(false);
    }
  };

  const remove = async (path: string) => {
    setError(null);
    try {
      await removeProject(path);
      await onRefreshProjects();
      if (path !== currentPath) return;
      setSelectedPath(workspacePath);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const start = async () => {
    if (selected === null || creating) return;
    setCreating(true);
    setError(null);
    try {
      await onStart(selected.path);
    } catch (cause) {
      setError(errorMessage(cause));
      setCreating(false);
    }
  };

  return (
    <main className="new-session">
      <div className="new-session-body">
        <button
          type="button"
          className="icon-button menu-button"
          onClick={onOpenSessions}
          aria-label="Open sessions"
        >
          Sessions
        </button>
        <h1>Start a chat</h1>
        <p className="new-session-lead">
          Pick the directory the harness should work in. Tools, shell commands, and subagents run
          there.
        </p>

        {projectsError !== null && (
          <p className="new-session-error" role="alert">
            {projectsError}{' '}
            <button
              type="button"
              className="project-retry"
              onClick={() => void onRefreshProjects()}
            >
              Retry
            </button>
          </p>
        )}

        {projectsLoaded ? (
          <fieldset className="project-list" disabled={creating}>
            <legend className="sr-only">Project</legend>
            {projects.map((project) => (
              <div
                key={project.path}
                className={
                  project.path === currentPath ? 'project-row project-row-selected' : 'project-row'
                }
              >
                <label className="project-choice">
                  <input
                    type="radio"
                    name="project"
                    value={project.path}
                    checked={project.path === currentPath}
                    disabled={!project.exists}
                    onChange={() => setSelectedPath(project.path)}
                  />
                  <span className="project-text">
                    <span className="project-name">
                      {project.name}
                      {!project.exists && <span className="project-missing">missing</span>}
                    </span>
                    <span className="project-path">{project.path}</span>
                  </span>
                </label>
                {!project.builtin && (
                  <button
                    type="button"
                    className="project-remove"
                    onClick={() => void remove(project.path)}
                    aria-label={`Remove ${project.name}`}
                  >
                    Remove
                  </button>
                )}
              </div>
            ))}
          </fieldset>
        ) : (
          <p className="new-session-note">Loading projects…</p>
        )}

        <form
          className="project-add"
          onSubmit={(event) => {
            event.preventDefault();
            void add();
          }}
        >
          <label className="sr-only" htmlFor="project-path">
            Project directory
          </label>
          <input
            id="project-path"
            type="text"
            value={draft}
            placeholder="/absolute/path/to/a/directory"
            spellCheck={false}
            onChange={(event) => setDraft(event.target.value)}
          />
          <button type="submit" disabled={adding || draft.trim() === ''}>
            {adding ? 'Adding…' : 'Add project'}
          </button>
        </form>

        {error !== null && (
          <p className="new-session-error" role="alert">
            {error}
          </p>
        )}

        <button
          type="button"
          className="new-session-start"
          onClick={() => void start()}
          disabled={!canStart}
        >
          {creating ? 'Starting…' : 'Start chat'}
        </button>
      </div>
    </main>
  );
}

function defaultProjectPath(projects: ProjectView[], sessions: SessionSummary[]): string | null {
  let newest: SessionSummary | null = null;
  for (const session of sessions) {
    if (newest === null || session.created_at > newest.created_at) newest = session;
  }
  if (newest !== null && newest.cwd !== null) {
    const match = projects.find((project) => project.path === newest.cwd);
    if (match !== undefined) return match.path;
  }
  return projects.find((project) => project.builtin)?.path ?? null;
}
