// Issue #41: the approvals inbox and the pending count in the nav. Component tests of the whole
// app against a fake API and fake sockets. Since #84 an approval is picked in the pending list and
// decided in the detail panel next to it.

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
    takes_messages: true,
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

/** Answers the approvals list: nothing is decided, so `pending` for all and for `?status=pending`. */
function approvalsList(url: URL, pending: Approval[]): Response {
  const status = url.searchParams.get('status');
  return new Response(JSON.stringify(status === null || status === 'pending' ? pending : []));
}

/** A fake API: pending approvals, runs by id, and resolve answering `resolve`. */
function serve(pending: Approval[], runs: RunSummary[], resolve: () => Response | Promise<Response> = () => new Response(null, { status: 202 })) {
  const calls: Call[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = new URL(String(input), 'http://kestrel.test');
    const method = init?.method ?? 'GET';
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : null;
    calls.push({ method, path: url.pathname, body });
    if (url.pathname === '/api/approvals') return approvalsList(url, pending);
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

/** The items of the pending list. */
function rows(): HTMLElement[] {
  return within(screen.getByRole('list', { name: 'Pending approvals' })).queryAllByRole('listitem');
}

/** The item of approval `id` in the pending list. */
const item = (id: string) => rows().find((x) => x.dataset.id === id) as HTMLElement;

/** Selects approval `id` in the pending list. */
async function select(u: ReturnType<typeof user>, id: string) {
  await u.click(within(item(id)).getByRole('button'));
}

/** The detail panel of the selected approval. */
const detail = () => screen.getByRole('region', { name: /asks/ });

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
    const pushed = approval(lead, { action: 'git push origin main', requested_at: new Date(NOW - 300_000).toISOString() });
    const removed = approval(kid, { action: 'rm -rf build', requested_at: new Date(NOW - 45_000).toISOString() });
    const mailed = approval(other, { action: 'send email to the team', requested_at: new Date(NOW - 125_000).toISOString() });
    show([pushed, removed, mailed], [lead, kid, other]);
    await vi.waitFor(() => expect(rows()[0]).toHaveTextContent('fixer'));
    const list = rows();
    expect(list).toHaveLength(3);
    expect(list.map((r) => r.querySelector('.action')?.textContent)).toEqual([
      'rm -rf build',
      'send email to the team',
      'git push origin main',
    ]);
    expect(list[0]).toHaveTextContent('waiting 45s');
    expect(list[1]).toHaveTextContent('waiting 2m 05s');
    // The first is selected: its detail has the run, its root and the payload.
    const first = detail();
    expect(first).toHaveTextContent('fixer');
    expect(first).toHaveTextContent('rm -rf build');
    expect(within(first).getByRole('link', { name: /^Run / })).toHaveAttribute('href', `#/runs/${kid.id}`);
    expect(within(first).getByRole('link', { name: /^Root / })).toHaveAttribute('href', `#/runs/${lead.id}`);
    expect(first).toHaveTextContent('waiting 45s');
    const u = user();
    await select(u, mailed.id);
    expect(detail()).toHaveTextContent('waiting 2m 05s');
    // A root's own approval has no separate root link.
    await select(u, pushed.id);
    expect(within(detail()).getByRole('link', { name: /^Run / })).toHaveAttribute('href', `#/runs/${lead.id}`);
    expect(within(detail()).queryByRole('link', { name: /^Root / })).not.toBeInTheDocument();
  });

  // 2
  it('adds an approval that arrives live at the top, counts it in the nav, and leaves focus alone', async () => {
    const r = run({ agent: 'lead' });
    const second = approval(r);
    const { sockets } = show([approval(r), second, approval(r)], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(3));
    expect(badge()).toBe('Approvals 3 pending');
    await select(user(), second.id);
    const approve = within(detail()).getByRole('button', { name: 'Approve' });
    approve.focus();
    const s = feed(sockets);
    s.open();
    s.event(9, event(r.id, { kind: 'approval_requested', id: 'new-1', action: 'deploy to staging' }, NOW + 1_000));
    await vi.waitFor(() => expect(rows()).toHaveLength(4));
    expect(rows()[0]).toHaveTextContent('deploy to staging');
    expect(badge()).toBe('Approvals 4 pending');
    expect(document.activeElement).toBe(approve);
    expect((document.activeElement as HTMLElement).closest('section')?.dataset.id).toBe(second.id);
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
    const busy = within(detail()).getByRole('button', { name: 'Approving' });
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
    await select(u, a.id);
    await u.click(within(detail()).getByRole('button', { name: 'Reject' }));
    await u.type(within(detail()).getByLabelText('Reason (optional)'), 'not on a Friday');
    await u.click(within(detail()).getByRole('button', { name: 'Confirm reject' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${a.id}/reject`, body: { reason: 'not on a Friday' } },
    ]);
    await select(u, b.id);
    await u.click(within(detail()).getByRole('button', { name: 'Reject' }));
    await u.click(within(detail()).getByRole('button', { name: 'Confirm reject' }));
    expect(posts(calls)[1]).toEqual({ method: 'POST', path: `/api/approvals/${b.id}/reject`, body: null });
    // Going back to the first keeps its rejection in progress.
    await select(u, a.id);
    expect(within(detail()).getByRole('button', { name: 'Rejecting' })).toBeDisabled();
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
    const panel = await vi.waitFor(() => detail());
    expect(panel).toHaveTextContent('Which branch should I release from?');
    expect(within(panel).queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    const u = user();
    await u.click(within(panel).getByRole('button', { name: 'Send answer' }));
    expect(within(panel).getByRole('alert')).toHaveTextContent('Write an answer first.');
    expect(posts(calls)).toEqual([]);
    await u.type(within(panel).getByLabelText('Answer'), 'release/2.4');
    await u.click(within(panel).getByRole('button', { name: 'Send answer' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${q.id}/approve`, body: { payload: 'release/2.4' } },
    ]);
    expect(within(panel).getByRole('button', { name: 'Sending answer' })).toBeDisabled();
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
    const panel = detail();
    expect(panel.textContent?.includes(long)).toBe(false);
    await user().click(toggle);
    expect(panel.textContent?.includes(long)).toBe(true);
    expect(within(panel).getByRole('button', { name: 'Show less' })).toHaveAttribute('aria-expanded', 'true');
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
    const panel = detail();
    expect(await within(panel).findByRole('alert')).toHaveTextContent('Failed to fetch');
    expect(within(panel).getByRole('button', { name: 'Approve' })).toBeEnabled();
    await u.click(within(panel).getByRole('button', { name: 'Retry' }));
    expect(posts(calls)).toHaveLength(2);
    expect(posts(calls)[1]?.path).toBe(`/api/approvals/${a.id}/approve`);
  });

  // 11
  it('marks an approval whose run was cancelled, with only Dismiss', async () => {
    const r = run();
    const a = approval(r);
    const { sockets } = show([a, approval(run())], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const u = user();
    await select(u, a.id);
    const s = feed(sockets);
    s.open();
    s.event(3, event(r.id, { kind: 'status_changed', from: { Suspended: { Approval: a.id } }, to: 'Cancelled' }));
    await vi.waitFor(() => expect(detail()).toHaveTextContent('Run cancelled'));
    expect(item(a.id)).toHaveTextContent('Command, run cancelled');
    expect(within(detail()).getAllByRole('button').map((b) => b.textContent?.trim())).toEqual(['Dismiss']);
    expect(badge()).toBe('Approvals 1 pending');
    await u.click(within(detail()).getByRole('button', { name: 'Dismiss' }));
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
    await u.click(screen.getByRole('button', { name: /Which branch/ }));
    await u.click(within(detail()).getByRole('button', { name: 'Reject' }));
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
    // The list fetched on open still has `a` (it was taken before the resolve was seen). Three
    // lists: pending at start, every approval for the decided list, and pending again on open.
    await vi.waitFor(() => expect(calls.filter((c) => c.path === '/api/approvals').length).toBeGreaterThanOrEqual(3));
    await vi.advanceTimersByTimeAsync(0);
    expect(rows().map((x) => x.dataset.id)).toEqual([b.id]);
  });

  it('shows a load error with Retry', async () => {
    let fail = true;
    const r = run();
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = new URL(String(input), 'http://kestrel.test');
      if (url.pathname === '/api/approvals' && url.searchParams.get('status') === 'pending' && fail) {
        fail = false;
        return new Response('{"error":"database is locked"}', { status: 500 });
      }
      if (url.pathname === '/api/approvals') return approvalsList(url, [approval(r)]);
      // The sidebar's count of active runs.
      if (url.pathname === '/api/runs') return new Response('[]');
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
    within(detail()).getByRole('button', { name: 'Approve' }).focus();
    await u.keyboard('{Enter}');
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    await vi.waitFor(() => expect(rows()).toHaveLength(1));
    expect((document.activeElement as HTMLElement).closest('li')?.dataset.id).toBe(b.id);
    within(detail()).getByRole('button', { name: 'Approve' }).focus();
    await u.keyboard('{Enter}');
    s.event(6, event(r.id, { kind: 'approval_resolved', id: b.id, approved: true }));
    await vi.waitFor(() => expect(screen.getByText('Nothing waiting for you')).toBeInTheDocument());
    expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1, name: 'Approvals' }));
  });

  it('moves focus to the item now in its place when a focused item that was not selected goes', async () => {
    const r = run();
    const a = approval(r, { requested_at: new Date(NOW - 10_000).toISOString() });
    const b = approval(r, { requested_at: new Date(NOW - 20_000).toISOString() });
    const c = approval(r, { requested_at: new Date(NOW - 30_000).toISOString() });
    const { sockets } = show([a, b, c], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(3));
    within(item(b.id)).getByRole('button').focus();
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: b.id, approved: true }));
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    expect((document.activeElement as HTMLElement).closest('li')?.dataset.id).toBe(c.id);
    expect(within(item(a.id)).getByRole('button')).toHaveAttribute('aria-pressed', 'true');
  });

  it('a 409 because the run stopped waiting marks the row ended instead of resolved', async () => {
    const r = run();
    const a = approval(r);
    show([a], [r], () => new Response(JSON.stringify({ error: `run ${r.id} is not waiting on approval ${a.id}` }), { status: 409 }));
    await user().click(await screen.findByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(detail()).toHaveTextContent('Run ended'));
    expect(within(detail()).getAllByRole('button').map((x) => x.textContent?.trim())).toEqual(['Dismiss']);
    expect(rows()[0]).toHaveTextContent('Command, run ended');
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
    await u.click(within(detail()).getByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(status).toHaveTextContent('Already resolved by someone else'));
    const seen: string[] = [];
    const watch = new MutationObserver(() => seen.push(status.textContent ?? ''));
    watch.observe(status, { childList: true, characterData: true, subtree: true });
    await u.click(within(detail()).getByRole('button', { name: 'Approve' }));
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
    expect(document.activeElement).toBe(screen.getByLabelText('Reason (optional)'));
    await u.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Reject' }));
    await u.click(screen.getByRole('button', { name: 'Reject' }));
    await u.type(screen.getByLabelText('Reason (optional)'), 'ask the release manager');
    await u.click(screen.getByRole('button', { name: 'Confirm reject' }));
    expect(posts(calls)).toEqual([
      { method: 'POST', path: `/api/approvals/${q.id}/reject`, body: { reason: 'ask the release manager' } },
    ]);
  });

  it('describes the buttons by who asks, so approvals are told apart', async () => {
    const lead = run({ agent: 'lead' });
    const kid = run({ agent: 'kid' });
    const fromKid = approval(kid, { requested_at: new Date(NOW - 20_000).toISOString() });
    show([approval(lead, { requested_at: new Date(NOW - 10_000).toISOString() }), fromKid], [lead, kid]);
    await vi.waitFor(() => expect(rows()[1]).toHaveTextContent('kid'));
    const description = () => {
      const approve = within(detail()).getByRole('button', { name: 'Approve' });
      return document.getElementById(approve.getAttribute('aria-describedby') ?? '')?.textContent;
    };
    await vi.waitFor(() => expect(description()).toMatch(/lead/));
    await select(user(), fromKid.id);
    expect(description()).toMatch(/kid/);
  });

  it('asks again for a run whose details failed to load', async () => {
    const r = run({ agent: 'planner' });
    let fails = 1;
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = new URL(String(input), 'http://kestrel.test');
      if (url.pathname === '/api/approvals') return approvalsList(url, [approval(r)]);
      // The sidebar's count of active runs.
      if (url.pathname === '/api/runs') return new Response('[]');
      if (fails-- > 0) return new Response('{"error":"busy"}', { status: 503 });
      return new Response(JSON.stringify(r));
    };
    const sockets = fakeSockets();
    window.location.hash = '#/approvals';
    render(App, { fetch, sockets: sockets.factory });
    await vi.waitFor(() => expect(rows()).toHaveLength(1));
    expect(rows()[0]).toHaveTextContent('agent not known yet');
    await vi.advanceTimersByTimeAsync(6_000);
    expect(await within(rows()[0] as HTMLElement).findByText('planner')).toBeInTheDocument();
  });
});

describe('Approvals inbox: focus inside the detail panel', () => {
  it('opening Reject or Edit focuses its field, and Cancel goes back to the button', async () => {
    const r = run();
    show([approval(r)], [r]);
    const u = user();
    const reject = await screen.findByRole('button', { name: 'Reject' });
    reject.focus();
    await u.keyboard('{Enter}');
    expect(document.activeElement).toBe(screen.getByLabelText('Reason (optional)'));
    await u.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Reject' }));
    screen.getByRole('button', { name: 'Edit and approve' }).focus();
    await u.keyboard('{Enter}');
    expect(document.activeElement).toBe(screen.getByLabelText('Edited action'));
    await u.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Edit and approve' }));
  });

  it('when the run ends while a button of its panel is focused, focus goes to Dismiss', async () => {
    const r = run();
    const a = approval(r);
    const { sockets } = show([a], [r]);
    const approve = await screen.findByRole('button', { name: 'Approve' });
    approve.focus();
    const s = feed(sockets);
    s.open();
    s.event(3, event(r.id, { kind: 'status_changed', from: { Suspended: { Approval: a.id } }, to: 'Cancelled' }));
    await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Dismiss' })));
  });

  it('dismissing the selected ended approval moves focus to the next item', async () => {
    const r = run();
    const a = approval(r, { requested_at: new Date(NOW - 10_000).toISOString() });
    const b = approval(run(), { requested_at: new Date(NOW - 20_000).toISOString() });
    const { sockets } = show([a, b], [r]);
    await vi.waitFor(() => expect(rows()).toHaveLength(2));
    const s = feed(sockets);
    s.open();
    s.event(3, event(r.id, { kind: 'status_changed', from: 'Running', to: { Failed: 'boom' } }));
    const dismiss = await within(detail()).findByRole('button', { name: 'Dismiss' });
    await user().click(dismiss);
    expect(rows().map((x) => x.dataset.id)).toEqual([b.id]);
    expect((document.activeElement as HTMLElement).closest('li')?.dataset.id).toBe(b.id);
  });

  it('Retry does not send an answer that was cleared', async () => {
    const r = run();
    const q = approval(r, { question: true, action: 'Which branch?' });
    let fail = true;
    const { calls } = show([q], [r], () => {
      if (fail) {
        fail = false;
        throw new TypeError('Failed to fetch');
      }
      return new Response(null, { status: 202 });
    });
    const u = user();
    await u.type(await screen.findByLabelText('Answer'), 'main');
    await u.click(screen.getByRole('button', { name: 'Send answer' }));
    await screen.findByText('Failed to fetch');
    await u.clear(screen.getByLabelText('Answer'));
    await u.click(screen.getByRole('button', { name: 'Retry' }));
    expect(posts(calls)).toHaveLength(1);
    expect(screen.getByText('Write an answer first.')).toBeInTheDocument();
  });

  it('says "Rejecting" until the rejection is seen', async () => {
    const r = run();
    show([approval(r)], [r]);
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Reject' }));
    await u.click(screen.getByRole('button', { name: 'Confirm reject' }));
    expect(screen.getByRole('button', { name: 'Rejecting' })).toBeDisabled();
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

  it('dates a row the list brings for a run already seen to end, with the time the run ended', () => {
    const state = new ApprovalsState();
    const token = state.beginSync();
    state.apply(event('r1', { kind: 'status_changed', from: 'Running', to: 'Cancelled' }, NOW - 5_000));
    state.load([a1], token);
    expect(state.rows.a1?.ended).toBe('cancelled');
    expect(state.decidedAt.a1).toBe(new Date(NOW - 5_000).toISOString());
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

  it('remembers when each approval was seen resolved: the event time, or now for a decision accepted here (PR #85 review)', () => {
    const state = new ApprovalsState();
    state.load([a1, a2], state.beginSync());
    state.apply(event('r1', { kind: 'approval_resolved', id: 'a1', approved: true }, NOW + 5_000));
    // One never listed here too.
    state.apply(event('r1', { kind: 'approval_resolved', id: 'gone', approved: false }, NOW + 6_000));
    expect(state.decidedAt).toEqual({ a1: new Date(NOW + 5_000).toISOString(), gone: new Date(NOW + 6_000).toISOString() });
    const t0 = Date.now();
    state.accepted('a2');
    const at = Date.parse(state.decidedAt.a2 ?? '');
    expect(at).toBeGreaterThanOrEqual(t0);
    expect(at).toBeLessThanOrEqual(Date.now());
    // The event, when it comes, says when it really was.
    state.apply(event('r1', { kind: 'approval_resolved', id: 'a2', approved: true }, NOW + 9_000));
    expect(state.decidedAt.a2).toBe(new Date(NOW + 9_000).toISOString());
  });

  it('counts each change that can change the decided list, and nothing else (PR #85 review)', () => {
    const state = new ApprovalsState();
    state.load([a1, a2, { ...a2, id: 'a3', run_id: 'r3' }], state.beginSync());
    const changes = () => state.decidedChanges;
    expect(changes()).toBe(0);
    state.apply(event('r1', { kind: 'approval_resolved', id: 'a1', approved: true }));
    expect(changes()).toBe(1);
    // A run with no pending row ending changes nothing here.
    state.apply(event('r9', { kind: 'status_changed', from: 'Running', to: 'Completed' }));
    expect(changes()).toBe(1);
    state.apply(event('r1', { kind: 'status_changed', from: 'Running', to: 'Completed' }));
    expect(changes()).toBe(2);
    state.endRow('a3');
    expect(changes()).toBe(3);
    state.dismiss('a2');
    expect(changes()).toBe(4);
    // A row no longer there.
    state.endRow('a2');
    state.dismiss('a2');
    expect(changes()).toBe(4);
  });
});
