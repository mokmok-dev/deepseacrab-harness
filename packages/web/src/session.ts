export function readSessionId(): string {
  return window.location.hash.slice(1);
}

export function showNewSession(): void {
  if (window.location.hash === '') return;
  window.location.hash = '';
}

export function shortSessionId(id: string): string {
  const bare = id.startsWith('session-') ? id.slice('session-'.length) : id;
  return bare.slice(0, 8);
}
