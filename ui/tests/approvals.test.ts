// Issue #41: the approvals inbox and the pending count in the nav. Component tests of the whole
// app against a fake API and fake sockets.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import { ApprovalsState } from '../src/lib/approvals.svelte';
import type { Approval, Event, RunSummary } from '../src/lib/types';
import { fakeSockets, type FakeSocket } from './fake-socket';
import { ids, seriousViolations } from './axe';

const NOW = Date.parse('2026-11-21T10:00:00Z');

let n = 0;
const uuid = () => `00000000-0000-4000-8000-${String(++n).padStart(12, '0')}`;

function run(over: Partial<RunSummary> = {}): RunSummary {
  const id = over.id ?? uuid();
  return {
    id,
    agent: 'worker',
    status: { Suspended: { Approval: 'x' } },
    parent: null,
    root: id,
    depth: 0,
    spent: 0,
    limit: 2_000_000,
    sandbox_tier: 'gvisor',
    created_at: new Date(NOW - 600_000).toISOString(),
    ...over,
  };
}

function approval(r: RunSummary, over: Partial<Approval> = {}): Approval {
  return {
    id: uuid(),
    run_id: r.id,
    action: 'git push origin main',
    status: 'pending',
    requested_at: new Date(NOW - 60_000).toISOString(),
    question: false,
    ...over,
  };
}

interface Call {
  method: string;
  path: string;
  body: unknown;
}

/** A fake API: pending approvals, runs by id, and resolve answering `resolve`. */
function serve(pending: Approval[], runs: RunSummary[], resolve: () => Response | Promise<Response> = () => new Response(null, { status: 202 })) {
  const calls: Call[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = new URL(String(input), 'http://kestrel.test');
    const method = init?.method ?? 'GET';
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : null;
    calls.push({ method, path: url.pathname, body });
    if (url.pathname === '/api/approvals') return new Response(JSON.stringify(pending));
    if (url.pathname.startsWith('/api/approvals/')) return resolve();
    if (url.pathname === '/api/runs') return new Response(JSON.stringify(runs));
    const one = runs.find((r) => url.pathname === `/api/runs/${r.id}`);
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
  };
  return { fetch, calls };
}

function show(pending: Approval[], runs: RunSummary[], resolve?: () => Response | Promise<Response>) {
  const served = serve(pending, runs, resolve);
  const sockets = fakeSockets();
  window.location.hash = '#/approvals';
  const result = render(App, { fetch: served.fetch, sockets: sockets.factory });
  return { ...result, sockets, calls: served.calls };
}

/** The app's socket for approvals across all runs. */
function feed(sockets: ReturnType<typeof fakeSockets>): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes('/ws/events')).at(-1);
  if (!found) throw new Error('no events socket');
  return found;
}

function event(runId: string, kind: Event['kind'], at = NOW): Event {
  return { run_id: runId, seq: 1, ts: new Date(at).toISOString(), kind };
}

function rows(): HTMLElement[] {
  return within(screen.getByRole('list', { name: 'Pending approvals' })).queryAllByRole('listitem');
}

function badge(): string {
  return screen.getByRole('link', { name: /^Approvals/ }).textContent?.replace(/\s+/g, ' ').trim() ?? '';
}

const posts = (calls: Call[]) => calls.filter((c) => c.method === 'POST');

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

const user = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime });

describe('Approvals inbox', () => {
  // 1
  it('shows each pending approval with its agent, run, action, wait and payload, newest first', async () => {
    const lead = run({ agent: 'lead' });
    const kid = run({ agent: 'fixer', parent: lead.id, root: lead.id, depth: 1 });
    const other = run({ agent: 'mailer' });
    show(
      [
        approval(lead, { action: 'git push origin main', requested_at: new Date(NOW - 300_000).toISOString() }),
        approval(kid, { action: 'rm -rf build', requested_at: new Date(NOW - 45_000).toISOString() }),
        approval(other, { action: 'send email to the team', requested_at: new Date(NOW - 125_000).toISOString() }),
      ],
      [lead, kid, other],
    );
    await screen.findByText('fixer');
    const list = rows();
    expect(list).toHaveLength(3);
    expect(list.map((r) => r.querySelector('.action')?.textContent)).toEqual([
      'rm -rf build',
      'send email to the team',
      'git push origin main',
    ]);
    const first = list[0] as HTMLElement;
    expect(first).toHaveTextContent('fixer');
    expect(within(first).getByRole('link', { name: /^Run / })).toHaveAttribute('href', `#/runs/${kid.id}`);
    expect(within(first).getByRole('link', { name: /^Root / })).toHaveAttribute('href', `#/runs/${lead.id}`);
    expect(first).toHaveTextContent('waiting 45s');
    expect(list[1]).toHaveTextContent('waiting 2m 05s');
    // A root's own approval has no separate root link.
    expect(within(list[2] as HTMLElement).queryByRole('link', { name: /^Root / })).not.toBeInTheDocument();
  });

  // 2
  it('adds an approval that arrives live at the top, counts it in the nav, and leaves focus alone', async () => {
    const r = run({ agent: 'lead' });
    const { sockets } = show([approval(r), approval(r), approval(r)], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(3));
    expect(badge()).toBe('Approvals 3 pending');
    const approve = within(rows()[1] as HTMLElement).getByRole('button', { name: 'Approve' });
    approve.focus();
    const focusedId = approve.closest('li')?.dataset.id;
    const s = feed(sockets);
    s.open();
    s.event(9, event(r.id, { kind: 'approval_requested', id: 'new-1', action: 'deploy to staging' }, NOW + 1_000));
    await vi.waitFor(() => expect(rows()).toHaveLength(4));
    expect(rows()[0]).toHaveTextContent('deploy to staging');
    expect(badge()).toBe('Approvals 4 pending');
    expect(document.activeElement).toBe(approve);
    expect((document.activeElement as HTMLElement).closest('li')?.dataset.id).toBe(focusedId);
  });

  it('shows the count on every view', async () => {
    const r = run();
    const served = serve([approval(r), approval(r)], [r]);
    window.location.hash = '#/fleet';
    render(App, { fetch: served.fetch, sockets: fakeSockets().factory });
    await vi.waitFor(() => expect(badge()).toBe('Approvals 2 pending'));
  });

  // 3
  it('approves: posts, shows "Approving" disabled, and removes the row when the resolved event arrives', async () => {
    const r = run();
    const a = approval(r);
    const { sockets, calls } = show([a], [r]);
    const approve = await screen.findByRole('button', { name: 'Approve' });
    await user().click(approve);
    expect(posts(calls)).toEqual([{ method: 'POST', path: `/api/approvals/${a.id}/approve`, body: null }]);
    const busy = within(rows()[0] as HTMLElement).getByRole('button', { name: 'Approving' });
    expect(busy).toBeDisabled();
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    await vi.waitFor(() => expect(screen.queryByRole('list', { name: 'Pending approvals' })).not.toBeInTheDocument());
    expect(screen.getByText('Nothing waiting for you')).toBeInTheDocument();
    expect(badge()).toBe('Approvals');
  });

  // 4
  it('rejects with an optional reason', async () => {
    const r = run();
    const a = approval(r);
    const b = approval(r);
    const { sockets, calls } = show([a, b], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const u = user();
    const row = rows().find((x) => x.dataset.id === a.id) as HTMLElement;
    await u.click(within(row).getByRole('button', { name: 'Reject' }));
    await u.type(within(row).getByLabelText('Reason (optional)'), 'not on a Friday');
    await u.click(within(row).getByRole('button', { name: 'Confirm reject' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${a.id}/reject`, body: { reason: 'not on a Friday' } },
    ]);
    const other = rows().find((x) => x.dataset.id === b.id) as HTMLElement;
    await u.click(within(other).getByRole('button', { name: 'Reject' }));
    await u.click(within(other).getByRole('button', { name: 'Confirm reject' }));
    expect(posts(calls)[1]).toEqual({ method: 'POST', path: `/api/approvals/${b.id}/reject`, body: null });
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: a.id, approved: false, payload: 'not on a Friday' }));
    await vi.waitFor(() => expect(rows()).toHaveLength(1));
  });

  // 5
  it('edits and approves with the edited payload; Cancel sends nothing', async () => {
    const r = run();
    const a = approval(r, { action: 'git push origin main' });
    const { calls } = show([a], [r]);
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Edit and approve' }));
    const box = screen.getByLabelText('Edited action');
    expect(box).toHaveValue('git push origin main');
    await u.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByLabelText('Edited action')).not.toBeInTheDocument();
    expect(posts(calls)).toEqual([]);
    await u.click(screen.getByRole('button', { name: 'Edit and approve' }));
    await u.clear(screen.getByLabelText('Edited action'));
    await u.type(screen.getByLabelText('Edited action'), 'git push origin release');
    await u.click(screen.getByRole('button', { name: 'Approve edited' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${a.id}/approve`, body: { payload: 'git push origin release' } },
    ]);
  });

  // 6
  it('says when someone else resolved it first, and removes the row', async () => {
    const r = run();
    const a = approval(r);
    show([a], [r], () => new Response(JSON.stringify({ error: 'approval is already resolved' }), { status: 409 }));
    await user().click(await screen.findByRole('button', { name: 'Approve' }));
    expect(await screen.findByText('Already resolved by someone else')).toBeInTheDocument();
    expect(screen.queryByRole('list', { name: 'Pending approvals' })).not.toBeInTheDocument();
  });

  // 7
  it('answers a question with text, and blocks an empty answer', async () => {
    const r = run({ agent: 'planner' });
    const q = approval(r, { action: 'Which branch should I release from?', question: true });
    const { calls } = show([q], [r]);
    const row = await vi.waitFor(() => rows()[0] as HTMLElement);
    expect(row).toHaveTextContent('Which branch should I release from?');
    expect(within(row).queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    const u = user();
    await u.click(within(row).getByRole('button', { name: 'Send answer' }));
    expect(within(row).getByRole('alert')).toHaveTextContent('Write an answer first.');
    expect(posts(calls)).toEqual([]);
    await u.type(within(row).getByLabelText('Answer'), 'release/2.4');
    await u.click(within(row).getByRole('button', { name: 'Send answer' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${q.id}/approve`, body: { payload: 'release/2.4' } },
    ]);
  });

  // 8
  it('says when nothing is waiting', async () => {
    show([], []);
    expect(await screen.findByText('Nothing waiting for you')).toBeInTheDocument();
    expect(badge()).toBe('Approvals');
  });

  // 9
  it('cuts a long payload with "Show all"', async () => {
    const r = run();
    const long = 'diff --git '.padEnd(50_000, 'x');
    show([approval(r, { action: long })], [r]);
    const toggle = await screen.findByRole('button', { name: 'Show all' });
    const row = rows()[0] as HTMLElement;
    expect(row.textContent?.includes(long)).toBe(false);
    await user().click(toggle);
    expect(row.textContent?.includes(long)).toBe(true);
    expect(within(row).getByRole('button', { name: 'Show less' })).toHaveAttribute('aria-expanded', 'true');
  });

  // 10
  it('puts the row back and offers Retry after a network error', async () => {
    const r = run();
    const a = approval(r);
    let fail = true;
    const { calls } = show([a], [r], () => {
      if (fail) {
        fail = false;
        throw new TypeError('Failed to fetch');
      }
      return new Response(null, { status: 202 });
    });
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Approve' }));
    const row = rows()[0] as HTMLElement;
    expect(await within(row).findByRole('alert')).toHaveTextContent('Failed to fetch');
    expect(within(row).getByRole('button', { name: 'Approve' })).toBeEnabled();
    await u.click(within(row).getByRole('button', { name: 'Retry' }));
    expect(posts(calls)).toHaveLength(2);
    expect(posts(calls)[1]?.path).toBe(`/api/approvals/${a.id}/approve`);
  });

  // 11
  it('marks an approval whose run was cancelled, with only Dismiss', async () => {
    const r = run();
    const a = approval(r);
    const { sockets } = show([a, approval(run())], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const s = feed(sockets);
    s.open();
    s.event(3, event(r.id, { kind: 'status_changed', from: { Suspended: { Approval: a.id } }, to: 'Cancelled' }));
    const row = await vi.waitFor(() => {
      const found = rows().find((x) => x.dataset.id === a.id) as HTMLElement;
      expect(found).toHaveTextContent('Run cancelled');
      return found;
    });
    expect(within(row).getAllByRole('button').map((b) => b.textContent?.trim())).toEqual(['Dismiss']);
    expect(badge()).toBe('Approvals 1 pending');
    await user().click(within(row).getByRole('button', { name: 'Dismiss' }));
    expect(rows()).toHaveLength(1);
  });

  // 12
  it('works from the keyboard and has no serious axe violations', async () => {
    const r = run();
    const a = approval(r);
    const { container, calls } = show([a, approval(r, { question: true, action: 'Which branch?' })], [r]);
    const approve = await screen.findByRole('button', { name: 'Approve' });
    const u = user();
    for (let i = 0; i < 40 && document.activeElement !== approve; i++) await u.tab();
    expect(document.activeElement).toBe(approve);
    await u.keyboard('{Enter}');
    expect(posts(calls)).toEqual([{ method: 'POST', path: `/api/approvals/${a.id}/approve`, body: null }]);
    await u.click(screen.getAllByRole('button', { name: 'Reject' })[0] as HTMLElement);
    vi.useRealTimers();
    expect(ids(await seriousViolations(container))).toEqual([]);
  });

  it('syncs again when the socket opens, without bringing back what was resolved meanwhile', async () => {
    const r = run();
    const a = approval(r);
    const b = approval(r);
    const { sockets, calls } = show([a, b], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const s = feed(sockets);
    s.event(4, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    s.open();
    // The list fetched on open still has `a` (it was taken before the resolve was seen).
    await vi.waitFor(() => expect(calls.filter((c) => c.path === '/api/approvals').length).toBeGreaterThanOrEqual(2));
    await vi.advanceTimersByTimeAsync(0);
    expect(rows().map((x) => x.dataset.id)).toEqual([b.id]);
  });

  it('shows a load error with Retry', async () => {
    let fail = true;
    const r = run();
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = new URL(String(input), 'http://kestrel.test');
      if (url.pathname === '/api/approvals' && fail) {
        fail = false;
        return new Response('{"error":"database is locked"}', { status: 500 });
      }
      if (url.pathname === '/api/approvals') return new Response(JSON.stringify([approval(r)]));
      return new Response(JSON.stringify(r));
    };
    window.location.hash = '#/approvals';
    render(App, { fetch, sockets: fakeSockets().factory });
    expect(await screen.findByRole('alert')).toHaveTextContent('database is locked');
    await user().click(screen.getByRole('button', { name: 'Retry' }));
    await vi.waitFor(() => expect(rows()).toHaveLength(1));
  });
});

describe('Approvals inbox: focus, refusals and retries', () => {
  it('moves focus to the next approval when the focused one goes, and to the heading when none is left', async () => {
    const r = run();
    const a = approval(r, { requested_at: new Date(NOW - 10_000).toISOString() });
    const b = approval(r, { requested_at: new Date(NOW - 20_000).toISOString() });
    const { sockets } = show([a, b], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const u = user();
    const first = within(rows()[0] as HTMLElement).getByRole('button', { name: 'Approve' });
    first.focus();
    await u.keyboard('{Enter}');
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    await vi.waitFor(() => expect(rows()).toHaveLength(1));
    expect((document.activeElement as HTMLElement).closest('li')?.dataset.id).toBe(b.id);
    await u.keyboard('{Enter}');
    s.event(6, event(r.id, { kind: 'approval_resolved', id: b.id, approved: true }));
    await vi.waitFor(() => expect(screen.getByText('Nothing waiting for you')).toBeInTheDocument());
    expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1, name: 'Approvals' }));
  });

  it('a 409 because the run stopped waiting marks the row ended instead of resolved', async () => {
    const r = run();
    const a = approval(r);
    show([a], [r], () => new Response(JSON.stringify({ error: `run ${r.id} is not waiting on approval ${a.id}` }), { status: 409 }));
    await user().click(await screen.findByRole('button', { name: 'Approve' }));
    const row = await vi.waitFor(() => {
      const found = rows()[0] as HTMLElement;
      expect(found).toHaveTextContent('Run ended');
      return found;
    });
    expect(within(row).getAllByRole('button').map((x) => x.textContent?.trim())).toEqual(['Dismiss']);
    expect(screen.queryByText('Already resolved by someone else')).not.toBeInTheDocument();
  });

  it('keeps the notice region in the page, and says it again for a second 409', async () => {
    const r = run();
    const a = approval(r);
    const b = approval(r);
    show([a, b], [r], () => new Response(JSON.stringify({ error: 'approval is resolved already' }), { status: 409 }));
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const status = screen.getByRole('status');
    expect(status).toHaveTextContent('');
    const u = user();
    await u.click(within(rows()[0] as HTMLElement).getByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(status).toHaveTextContent('Already resolved by someone else'));
    const seen: string[] = [];
    const watch = new MutationObserver(() => seen.push(status.textContent ?? ''));
    watch.observe(status, { childList: true, characterData: true, subtree: true });
    await u.click(within(rows()[0] as HTMLElement).getByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(seen.at(-1)).toBe('Already resolved by someone else'));
    // It was cleared first, so screen readers hear it again.
    expect(seen).toContain('');
    watch.disconnect();
  });

  it('Retry sends what the fields hold now', async () => {
    const r = run();
    const a = approval(r);
    let fail = true;
    const { calls } = show([a], [r], () => {
      if (fail) {
        fail = false;
        throw new TypeError('Failed to fetch');
      }
      return new Response(null, { status: 202 });
    });
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Reject' }));
    await u.type(screen.getByLabelText('Reason (optional)'), 'first');
    await u.click(screen.getByRole('button', { name: 'Confirm reject' }));
    await screen.findByRole('alert');
    await u.clear(screen.getByLabelText('Reason (optional)'));
    await u.type(screen.getByLabelText('Reason (optional)'), 'second');
    await u.click(screen.getByRole('button', { name: 'Retry' }));
    expect(posts(calls).map((c) => c.body)).toEqual([{ reason: 'first' }, { reason: 'second' }]);
  });

  it('rejecting a question asks for a reason first, like any approval', async () => {
    const r = run();
    const q = approval(r, { question: true, action: 'Which branch?' });
    const { calls } = show([q], [r]);
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Reject' }));
    expect(posts(calls)).toEqual([]);
    await u.type(screen.getByLabelText('Reason (optional)'), 'ask the release manager');
    await u.click(screen.getByRole('button', { name: 'Confirm reject' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${q.id}/reject`, body: { reason: 'ask the release manager' } },
    ]);
  });

  it('describes each row\'s buttons by who asks, so rows are told apart', async () => {
    const lead = run({ agent: 'lead' });
    const kid = run({ agent: 'kid' });
    show([approval(lead), approval(kid)], [lead, kid]);
    await screen.findByText('kid');
    const approves = screen.getAllByRole('button', { name: 'Approve' });
    const described = approves.map((b) => document.getElementById(b.getAttribute('aria-describedby') ?? '')?.textContent);
    expect(described[0]).not.toEqual(described[1]);
    expect(described.join(' ')).toMatch(/lead/);
  });

  it('asks again for a run whose details failed to load', async () => {
    const r = run({ agent: 'planner' });
    let fails = 1;
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = new URL(String(input), 'http://kestrel.test');
      if (url.pathname === '/api/approvals') return new Response(JSON.stringify([approval(r)]));
      if (fails-- > 0) return new Response('{"error":"busy"}', { status: 503 });
      return new Response(JSON.stringify(r));
    };
    const sockets = fakeSockets();
    window.location.hash = '#/approvals';
    render(App, { fetch, sockets: sockets.factory });
    await vi.waitFor(() => expect(rows()).toHaveLength(1));
    expect(rows()[0]).toHaveTextContent('agent not known yet');
    await vi.advanceTimersByTimeAsync(6_000);
    expect(await screen.findByText('planner')).toBeInTheDocument();
  });
});

describe('ApprovalsState', () => {
  const a1: Approval = {
    id: 'a1', run_id: 'r1', action: 'x', status: 'pending', requested_at: '2026-11-21T10:00:00Z', question: false,
  };
  const a2: Approval = { ...a1, id: 'a2', requested_at: '2026-11-21T10:00:05Z' };

  it('keeps what arrived after a sync began, and drops what the server no longer has', () => {
    const state = new ApprovalsState();
    state.load([a1], state.beginSync());
    const token = state.beginSync();
    state.apply(event('r1', { kind: 'approval_requested', id: 'live', action: 'y' }));
    state.load([a2], token);
    expect(Object.keys(state.rows).sort()).toEqual(['a2', 'live']);
    expect(state.count).toBe(2);
  });

  it('a request seen twice is one row, and a resolve for an unknown approval is remembered', () => {
    const state = new ApprovalsState();
    const token = state.beginSync();
    state.apply(event('r1', { kind: 'approval_resolved', id: 'a1', approved: true }));
    state.apply(event('r1', { kind: 'approval_requested', id: 'a1', action: 'x' }));
    state.load([a1], token);
    expect(state.count).toBe(0);
    state.apply(event('r1', { kind: 'approval_requested', id: 'b', action: 'y', question: true }));
    state.apply(event('r1', { kind: 'approval_requested', id: 'b', action: 'y' }));
    expect(state.rows.b?.approval.question).toBe(true);
    expect(state.count).toBe(1);
  });

  it('ignores a list older than one already loaded', () => {
    const state = new ApprovalsState();
    const older = state.beginSync();
    const newer = state.beginSync();
    state.load([], newer);
    state.load([a1], older);
    expect(state.count).toBe(0);
  });

  it('a run that ended while a list was on its way does not come back as decidable', () => {
    const state = new ApprovalsState();
    const token = state.beginSync();
    state.apply(event('r1', { kind: 'status_changed', from: 'Running', to: 'Cancelled' }));
    state.load([a1], token);
    expect(state.rows.a1?.ended).toBe('cancelled');
    expect(state.count).toBe(0);
  });

  it('marks rows of a run that ended, and ignores status changes that are not an end', () => {
    const state = new ApprovalsState();
    state.load([a1, { ...a2, run_id: 'r2' }], state.beginSync());
    state.apply(event('r1', { kind: 'status_changed', from: 'Running', to: 'Running' }));
    expect(state.rows.a1?.ended).toBeNull();
    state.apply(event('r1', { kind: 'status_changed', from: 'Running', to: { Failed: 'boom' } }));
    expect(state.rows.a1?.ended).toBe('ended');
    state.apply(event('r2', { kind: 'status_changed', from: 'Running', to: 'Cancelled' }));
    expect(state.rows.a2?.ended).toBe('cancelled');
    expect(state.count).toBe(0);
    state.apply(event('r1', { kind: 'agent_log', line: 'x' }));
    state.dismiss('a1');
    expect(Object.keys(state.rows)).toEqual(['a2']);
  });
});
