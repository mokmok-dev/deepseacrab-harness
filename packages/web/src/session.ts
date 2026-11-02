const createdHere = new Set<string>();

export function newSessionId(): string {
  const id = `session-${crypto.randomUUID()}`;
  createdHere.add(id);
  return id;
}

export function isNewSession(id: string): boolean {
  return createdHere.has(id);
}

export function markSessionStarted(id: string): void {
  createdHere.delete(id);
}

export function readSessionId(): string {
  return window.location.hash.slice(1);
}

export function readOrCreateSessionId(): string {
  const id = readSessionId();
  if (id !== '') return id;
  const created = newSessionId();
  window.location.replace(`#${created}`);
  return created;
}

export function startNewSession(): string {
  const id = newSessionId();
  window.location.hash = id;
  return id;
}

export function shortSessionId(id: string): string {
  const bare = id.startsWith('session-') ? id.slice('session-'.length) : id;
  return bare.slice(0, 8);
}
