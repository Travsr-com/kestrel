// A small typed client for the `kestrel-server` HTTP API.

import type { ApiError, Approval, ApprovalStatus, Event, RunSummary, TreeNode } from './types';

/** The server answered 401: no token, or a wrong one. `token` is the one the request carried. */
export class Unauthorized extends Error {
  constructor(readonly token: string | null) {
    super('token required');
    this.name = 'Unauthorized';
  }
}

/** Any other non-2xx answer. `message` is the server's `error` text when it sent one. */
export class ApiFailure extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'ApiFailure';
  }
}

export interface RunFilter {
  status?: string;
  agent?: string;
  root?: string;
}

export interface ApiOptions {
  fetch: typeof fetch;
  getToken: () => string | null;
  /** Prefix for every path. Empty (the default) means the page's own origin. */
  baseUrl?: string;
}

export type Api = ReturnType<typeof createApi>;

const seg = encodeURIComponent;

async function failure(res: Response, token: string | null): Promise<Error> {
  if (res.status === 401) return new Unauthorized(token);
  try {
    const body = (await res.json()) as Partial<ApiError>;
    if (typeof body.error === 'string') return new ApiFailure(res.status, body.error);
  } catch {
    // Not JSON: fall through to the status line.
  }
  return new ApiFailure(res.status, `HTTP ${res.status} ${res.statusText}`.trim());
}

export function createApi({ fetch, getToken, baseUrl = '' }: ApiOptions) {
  async function send(method: 'GET' | 'POST', path: string, body?: unknown): Promise<Response> {
    const headers: Record<string, string> = { accept: 'application/json' };
    const token = getToken();
    if (token !== null) headers.authorization = `Bearer ${token}`;
    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers['content-type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    const res = await fetch(`${baseUrl}${path}`, init);
    if (!res.ok) throw await failure(res, token);
    return res;
  }

  async function get<T>(path: string): Promise<T> {
    return (await (await send('GET', path)).json()) as T;
  }

  /** Commands answer 202 or 204, usually with no body, so the body is not read. */
  async function post(path: string, body?: unknown): Promise<void> {
    await send('POST', path, body);
  }

  return {
    listRuns(filter: RunFilter = {}): Promise<RunSummary[]> {
      const query = new URLSearchParams();
      for (const key of ['status', 'agent', 'root'] as const) {
        const value = filter[key];
        if (value !== undefined) query.set(key, value);
      }
      const qs = query.toString();
      return get(`/api/runs${qs ? `?${qs}` : ''}`);
    },
    getRun: (id: string): Promise<RunSummary> => get(`/api/runs/${seg(id)}`),
    tree: (id: string): Promise<TreeNode[]> => get(`/api/runs/${seg(id)}/tree`),
    events: (id: string, after: number): Promise<Event[]> => get(`/api/runs/${seg(id)}/events?after=${after}`),
    approvals: (status: ApprovalStatus): Promise<Approval[]> => get(`/api/approvals?status=${status}`),
    pause: (id: string) => post(`/api/runs/${seg(id)}/pause`),
    resume: (id: string) => post(`/api/runs/${seg(id)}/resume`),
    cancel: (id: string) => post(`/api/runs/${seg(id)}/cancel`),
    /** `usd` is a decimal string such as `"5.00"`, so no float rounding happens on the way. */
    setBudget: (id: string, usd: string) => post(`/api/runs/${seg(id)}/budget`, { usd }),
    message: (id: string, text: string) => post(`/api/runs/${seg(id)}/message`, { text }),
    /** `payload` is the edited action, for "edit and approve". */
    approve: (id: string, payload?: string) =>
      post(`/api/approvals/${seg(id)}/approve`, payload === undefined ? undefined : { payload }),
    reject: (id: string, reason?: string) =>
      post(`/api/approvals/${seg(id)}/reject`, reason === undefined ? undefined : { reason }),
  };
}
