export function workspaceName(path: string | null): string {
  if (path === null || path === '') return 'Workspace';
  const trimmed = path.replace(/[\\/]+$/, '');
  const cut = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  const name = trimmed.slice(cut + 1);
  return name === '' ? path : name;
}
