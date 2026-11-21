// Hash routes: `#/fleet`, `#/runs/<id>`, `#/approvals`. Hash routing needs no server support, so
// the embedded build works from any path.

export type Route =
  | { name: 'fleet' }
  | { name: 'approvals' }
  | { name: 'run'; id: string }
  | { name: 'notfound' };

export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, '');
  if (path === '' || path === '/' || path === '/fleet') return { name: 'fleet' };
  if (path === '/approvals') return { name: 'approvals' };
  const run = /^\/runs\/([^/]+)$/.exec(path);
  if (run?.[1]) {
    try {
      return { name: 'run', id: decodeURIComponent(run[1]) };
    } catch {
      return { name: 'notfound' };
    }
  }
  return { name: 'notfound' };
}

export function href(route: Exclude<Route, { name: 'notfound' }>): string {
  switch (route.name) {
    case 'fleet':
      return '#/fleet';
    case 'approvals':
      return '#/approvals';
    case 'run':
      return `#/runs/${encodeURIComponent(route.id)}`;
  }
}
