// Issue #84, scenarios 1 to 3: the sidebar of the restyled shell, and the active-run count it shows.

import { render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import { ActiveRuns } from '../src/lib/active.svelte';
import type { Approval, Event, RunSummary } from '../src/lib/types';
import { fakeSockets, type FakeSocket } from './fake-socket';
import { ids, seriousViolations } from './axe';

const NOW = Date.parse('2026-11-22T10:00:00Z');

let n = 0;
const uuid = () => `00000000-0000-4000-8000-${String(++n).padStart(12, '0')}`;

function run(over: Partial<RunSummary> = {}): RunSummary {
  const id = over.id ?? uuid();
  return {
    id,
    agent: 'worker',
    status: 'Running',
    parent: null,
    root: id,
    depth: 0,
    spent: 0,
    limit: 2_000_000,
    sandbox_tier: 'gvisor',
    created_at: new Date(NOW - 60_000).toISOString(),
    ...over,
  };
}

function approval(r: RunSummary): Approval {
  return {
    id: uuid(),
    run_id: r.id,
    action: 'git push origin main',
    status: 'pending',
    requested_at: new Date(NOW - 60_000).toISOString(),
    question: false,
  };
}

interface Served {
  runs: RunSummary[];
  pending: Approval[];
  /** Run statuses whose `?status=` request fails. */
  fail: string[];
  /** The search part of every `/api/runs` request. */
  runSearches: string[];
}

/** The lower-case status name the `?status=` filter takes. */
const statusOf = (r: RunSummary) =>
  typeof r.status === 'string' ? r.status.toLowerCase() : 'Suspended' in r.status ? 'suspended' : 'failed';

function serve(served: Served) {
  const fetch: typeof globalThis.fetch = async (input) => {
    const url = new URL(String(input), 'http://kestrel.test');
    if (url.pathname === '/api/approvals') return new Response(JSON.stringify(served.pending));
    if (url.pathname === '/api/runs') {
      served.runSearches.push(url.search);
      const status = url.searchParams.get('status');
      if (status !== null && served.fail.includes(status)) {
        return new Response('{"error":"database is locked"}', { status: 500 });
      }
      return new Response(JSON.stringify(served.runs.filter((r) => status === null || statusOf(r) === status)));
    }
    const one = served.runs.find((r) => url.pathname === `/api/runs/${r.id}`);
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
  };
  return fetch;
}

function show(hash: string, runs: RunSummary[], pending: Approval[] = []) {
  const sockets = fakeSockets();
  const served: Served = { runs, pending, fail: [], runSearches: [] };
  window.location.hash = hash;
  const result = render(App, { fetch: serve(served), sockets: sockets.factory });
  return { ...result, sockets, served };
}

/** What a screen reader says for `el`: its text without the parts hidden from it. */
function spoken(el: Element): string {
  const copy = el.cloneNode(true) as Element;
  copy.querySelectorAll('[aria-hidden="true"]').forEach((hidden) => hidden.remove());
  return copy.textContent?.replace(/\s+/g, ' ').trim() ?? '';
}

function feed(sockets: ReturnType<typeof fakeSockets>): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes('/ws/events')).at(-1);
  if (!found) throw new Error('no events socket');
  return found;
}

const nav = () => screen.getByRole('navigation', { name: 'Main' });
const item = (label: string) => {
  const found = within(nav())
    .getAllByRole('listitem')
    .find((li) => li.textContent?.trim().startsWith(label));
  if (!found) throw new Error(`no nav item ${label}`);
  return found;
};
const text = (el: Element) => el.textContent?.replace(/\s+/g, ' ').trim() ?? '';

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('sidebar', () => {
  // 1
  it('lists Fleet, Run and Approvals, and the later pages as items with their milestone that are not links', async () => {
    show('#/fleet', []);
    await screen.findByRole('heading', { level: 1, name: 'Fleet' });
    expect(within(item('Fleet')).getByRole('link')).toHaveAttribute('href', '#/fleet');
    expect(within(item('Approvals')).getByRole('link')).toHaveAttribute('href', '#/approvals');
    for (const [label, tag, said] of [
      ['Agents', 'M4', 'Agents, coming in M4'],
      ['Workflows', 'M4', 'Workflows, coming in M4'],
      ['Triggers', 'M4', 'Triggers, coming in M4'],
      ['Meetings', 'M5', 'Meetings, coming in M5'],
      ['Costs', 'M3', 'Costs, coming in M3'],
      ['Settings', 'Soon', 'Settings, coming soon'],
    ] as const) {
      const li = item(label);
      expect(within(li).queryByRole('link')).not.toBeInTheDocument();
      // The full phrase is read out; the short tag is only seen.
      expect(spoken(li)).toBe(said);
      const shown = li.querySelector('.tag');
      expect(shown).toHaveAttribute('aria-hidden', 'true');
      expect(shown?.textContent).toBe(tag);
    }
  });

  // 2
  it('counts active runs on Fleet and pending approvals on Approvals, and marks the current page', async () => {
    const a = run();
    const b = run({ status: { Suspended: 'Budget' } });
    const done = run({ status: 'Completed' });
    show('#/approvals', [a, b, done], [approval(a), approval(b), approval(b)]);
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 2 active'));
    await vi.waitFor(() => expect(text(within(item('Approvals')).getByRole('link'))).toBe('Approvals 3 pending'));
    expect(within(item('Approvals')).getByRole('link')).toHaveAttribute('aria-current', 'page');
    expect(within(item('Fleet')).getByRole('link')).not.toHaveAttribute('aria-current');
  });

  it('keeps the active count live from the events socket', async () => {
    const a = run();
    const { sockets } = show('#/approvals', [a]);
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 1 active'));
    const s = feed(sockets);
    expect(s.url).toContain('run_created');
    s.open();
    const ev = (runId: string, kind: Event['kind']): Event => ({ run_id: runId, seq: 1, ts: new Date(NOW).toISOString(), kind });
    s.event(1, ev('new-run', { kind: 'run_created', agent: 'worker' }));
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 2 active'));
    s.event(2, ev(a.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }));
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 1 active'));
  });

  it('asks only for live runs, one request per live status, never the whole history (PR #85 review)', async () => {
    const a = run();
    const b = run({ status: 'Queued' });
    const c = run({ status: { Suspended: 'Manual' } });
    const d = run({ status: 'Provisioning' });
    const { served } = show('#/approvals', [a, b, c, d, run({ status: 'Completed' }), run({ status: { Failed: 'x' } })]);
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 4 active'));
    expect([...served.runSearches].sort()).toEqual([
      '?status=provisioning',
      '?status=queued',
      '?status=running',
      '?status=suspended',
    ]);
  });

  it('leaves the active count as it was when one of the live-status requests fails', async () => {
    const a = run();
    const { served, sockets } = show('#/approvals', [a]);
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 1 active'));
    served.runs.push(run({ status: 'Queued' }), run());
    served.fail.push('running');
    feed(sockets).open();
    await vi.waitFor(() => expect(served.runSearches).toHaveLength(8));
    await vi.advanceTimersByTimeAsync(10);
    expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 1 active');
    // The next sync that succeeds brings the count up to date.
    served.fail.length = 0;
    feed(sockets).drop();
    await vi.advanceTimersByTimeAsync(1_000);
    feed(sockets).open();
    await vi.waitFor(() => expect(text(within(item('Fleet')).getByRole('link'))).toBe('Fleet 3 active'));
  });

  // 3
  it('shows Run as plain text until a run is opened, then links to that run', async () => {
    const a = run({ agent: 'lead' });
    show('#/fleet', [a]);
    await screen.findByRole('heading', { level: 1, name: 'Fleet' });
    expect(within(item('Run')).queryByRole('link')).not.toBeInTheDocument();
    expect(text(item('Run'))).toBe('Run, open one from Fleet');
    window.location.hash = `#/runs/${a.id}`;
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    await screen.findByRole('heading', { level: 1, name: 'lead' });
    const link = within(item('Run')).getByRole('link', { name: 'Run' });
    expect(link).toHaveAttribute('href', `#/runs/${a.id}`);
    expect(link).toHaveAttribute('aria-current', 'page');
    window.location.hash = '#/fleet';
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    await screen.findByRole('heading', { level: 1, name: 'Fleet' });
    expect(within(item('Run')).getByRole('link', { name: 'Run' })).not.toHaveAttribute('aria-current');
  });

  it('has no serious or critical axe violations', async () => {
    const { container } = show('#/fleet', [run()]);
    await screen.findByTestId('ready');
    expect(ids(await seriousViolations(container))).toEqual([]);
  });
});

describe('ActiveRuns', () => {
  it('counts runs that have not ended, from a list and then from events', () => {
    const s = new ActiveRuns();
    expect(s.loaded).toBe(false);
    s.load([run({ id: 'a' }), run({ id: 'b', status: { Failed: 'x' } }), run({ id: 'c', status: 'Queued' })]);
    expect(s.loaded).toBe(true);
    expect(s.count).toBe(2);
    const ev = (runId: string, kind: Event['kind']): Event => ({ run_id: runId, seq: 1, ts: '', kind });
    s.apply(ev('d', { kind: 'run_created', agent: 'w' }));
    expect(s.count).toBe(3);
    s.apply(ev('a', { kind: 'status_changed', from: 'Running', to: 'Cancelled' }));
    expect(s.count).toBe(2);
    // An ended run is not brought back by a late creation event or a list fetched before it ended.
    s.apply(ev('a', { kind: 'run_created', agent: 'w' }));
    s.load([run({ id: 'a' }), run({ id: 'c', status: 'Queued' }), run({ id: 'd' })]);
    expect(s.count).toBe(2);
    // Other events change nothing; a status change to a live status adds a run not seen yet.
    s.apply(ev('c', { kind: 'agent_message', text: 'hi' }));
    s.apply(ev('e', { kind: 'status_changed', from: 'Queued', to: 'Running' }));
    expect(s.count).toBe(3);
  });

  it('keeps a run created while a list was on its way, and drops a list older than one taken', () => {
    const s = new ActiveRuns();
    const first = s.begin();
    const second = s.begin();
    s.apply({ run_id: 'x', seq: 1, ts: '', kind: { kind: 'run_created', agent: 'w' } });
    s.load([run({ id: 'a' })], second);
    expect(s.count).toBe(2);
    s.load([], first);
    expect(s.count).toBe(2);
  });
});
