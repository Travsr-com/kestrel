// Issue #40: the Run view. Component tests against a fake API and fake sockets.

import { fireEvent, render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import Run from '../src/views/Run.svelte';
import { createApi } from '../src/lib/api';
import { addTokens, allowed, carved, formatSize, liveBelow, messageRefusal, parseUsd } from '../src/lib/run';
import type { Event, RunStatus, RunSummary } from '../src/lib/types';
import { fakeSockets, type FakeSocket } from './fake-socket';
import { ids, seriousViolations } from './axe';

const NOW = Date.parse('2026-11-21T10:00:00Z');

let n = 0;
function run(over: Partial<RunSummary> = {}): RunSummary {
  n += 1;
  const id = over.id ?? `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
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
    takes_messages: true,
    created_at: new Date(NOW - 60_000).toISOString(),
    ...over,
  };
}

interface Call {
  method: string;
  path: string;
  body: unknown;
}

/** A fake API: runs by id, the list filtered by root, and commands answering `command`. */
function serve(runs: RunSummary[], command: { status: number; error?: string } = { status: 202 }) {
  const calls: Call[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = new URL(String(input), 'http://kestrel.test');
    const method = init?.method ?? 'GET';
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : null;
    calls.push({ method, path: url.pathname, body });
    const parts = url.pathname.split('/').filter(Boolean);
    if (method === 'GET' && url.pathname === '/api/runs') {
      const root = url.searchParams.get('root');
      return new Response(JSON.stringify(runs.filter((r) => root === null || r.root === root)));
    }
    const one = runs.find((r) => r.id === parts[2]);
    if (!one) return new Response(JSON.stringify({ error: 'run not found' }), { status: 404 });
    if (method === 'GET') return new Response(JSON.stringify(one));
    if (command.status === 202) return new Response(null, { status: 202 });
    return new Response(JSON.stringify({ error: command.error ?? 'no' }), { status: command.status });
  };
  return { api: createApi({ fetch, getToken: () => null }), calls };
}

function show(id: string, runs: RunSummary[], command?: { status: number; error?: string }) {
  const { api, calls } = serve(runs, command);
  const sockets = fakeSockets();
  const result = render(Run, { id, api, onUnauthorized: () => {}, sockets: sockets.factory, base: 'ws://kestrel.test' });
  return { ...result, sockets, calls };
}

function event(runId: string, seq: number, kind: Event['kind']): Event {
  return { run_id: runId, seq, ts: new Date(NOW).toISOString(), kind };
}

/** The socket that follows one run's stream. */
function stream(sockets: ReturnType<typeof fakeSockets>, id: string): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes(`/ws/runs/${id}`)).at(-1);
  if (!found) throw new Error(`no stream socket for ${id}`);
  return found;
}

/** The socket that follows a tree. */
function tree(sockets: ReturnType<typeof fakeSockets>, root: string): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes(`/ws/tree/${root}`)).at(-1);
  if (!found) throw new Error(`no tree socket for ${root}`);
  return found;
}

function log(line: string): Event['kind'] {
  return { kind: 'agent_log', line };
}

function items(): string[] {
  return within(screen.getByRole('list', { name: 'Events' }))
    .queryAllByRole('listitem')
    .map((li) => li.textContent ?? '');
}

async function settle() {
  await vi.advanceTimersByTimeAsync(0);
}

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

const user = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime });

describe('Run view', () => {
  // 1
  it('replays past events in order from seq 0, then appends a live one', async () => {
    const r = run({ agent: 'lead' });
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('list', { name: 'Events' });
    const s = stream(sockets, r.id);
    expect(s.url).toBe(`ws://kestrel.test/ws/runs/${r.id}?after=0`);
    s.open();
    for (let i = 1; i <= 20; i++) s.event(i, event(r.id, i, log(`line ${i}`)));
    await settle();
    expect(items()).toHaveLength(20);
    expect(items()[0]).toContain('line 1');
    expect(items()[19]).toContain('line 20');
    s.event(21, event(r.id, 21, log('live')));
    await settle();
    expect(items()).toHaveLength(21);
    expect(items()[20]).toContain('live');
  });

  // 2
  it('resumes after the last seq on a reconnect and shows no event twice', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('list', { name: 'Events' });
    const first = stream(sockets, r.id);
    first.open();
    first.event(1, event(r.id, 1, log('one')));
    first.event(2, event(r.id, 2, log('two')));
    first.drop();
    expect(await screen.findByText('Reconnecting', { selector: '.link' })).toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(600);
    const second = stream(sockets, r.id);
    expect(second).not.toBe(first);
    expect(second.url).toContain('after=2');
    second.open();
    second.event(2, event(r.id, 2, log('two')));
    second.event(3, event(r.id, 3, log('three')));
    await settle();
    expect(items().map((t) => t.match(/one|two|three/)?.[0])).toEqual(['one', 'two', 'three']);
    expect(screen.queryByText(/Reconnecting/)).not.toBeInTheDocument();
  });

  // 3
  it('shows the tree nested, and collapsing a child hides its descendants', async () => {
    const root = run({ agent: 'lead' });
    const a = run({ agent: 'alpha', parent: root.id, root: root.id, depth: 1 });
    const b = run({ agent: 'beta', parent: root.id, root: root.id, depth: 1 });
    const g = run({ agent: 'gamma', parent: a.id, root: root.id, depth: 2 });
    show(root.id, [root, a, b, g, run({ agent: 'stranger' })]);
    const nav = await screen.findByRole('navigation', { name: 'Spawn tree' });
    await within(nav).findByRole('link', { name: /gamma/ });
    expect(within(nav).getAllByRole('link')).toHaveLength(4);
    expect(within(nav).queryByRole('link', { name: /stranger/ })).not.toBeInTheDocument();
    // gamma sits inside alpha's list, alpha and beta inside lead's.
    const alphaItem = within(nav).getByRole('link', { name: /alpha/ }).closest('li');
    expect(alphaItem).not.toBeNull();
    expect(within(alphaItem as HTMLElement).getByRole('link', { name: /gamma/ })).toBeInTheDocument();
    const toggle = within(alphaItem as HTMLElement).getByRole('button', { name: 'Children of alpha' });
    await user().click(toggle);
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
    expect(within(nav).queryByRole('link', { name: /gamma/ })).not.toBeInTheDocument();
    expect(within(nav).getByRole('link', { name: /beta/ })).toBeInTheDocument();
  });

  // 4
  it('adds a child spawned live under its parent, keeping the selection and the scroll', async () => {
    const root = run({ agent: 'lead' });
    const { sockets } = show(root.id, [root]);
    const nav = await screen.findByRole('navigation', { name: 'Spawn tree' });
    const box = screen.getByRole('list', { name: 'Events' }).parentElement as HTMLElement;
    Object.defineProperty(box, 'scrollHeight', { configurable: true, get: () => 1000 });
    Object.defineProperty(box, 'clientHeight', { configurable: true, get: () => 200 });
    box.scrollTop = 37;
    await fireEvent.scroll(box);
    const s = stream(sockets, root.id);
    s.open();
    const t = tree(sockets, root.id);
    t.open();
    // The parent's own stream carries the spawn too.
    const spawned: Event['kind'] = { kind: 'child_spawned', child: 'kid-1', agent: 'kid', budget: 300_000 };
    t.event(1, event(root.id, 1, spawned));
    s.event(1, event(root.id, 1, spawned));
    await settle();
    const kid = await within(nav).findByRole('link', { name: /kid/ });
    expect(kid.closest('ul')?.closest('li')).toBe(within(nav).getByRole('link', { name: /lead/ }).closest('li'));
    expect(within(nav).getByRole('link', { name: /lead/ })).toHaveAttribute('aria-current', 'page');
    expect(box.scrollTop).toBe(37);
    expect(within(nav).getByRole('link', { name: /kid/ })).toHaveAccessibleDescription('Queued $0.00');
  });

  // 5
  it('selecting a child shows its stream and puts it in the URL', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const sockets = fakeSockets();
    const served = serveFetch([root, kid]);
    window.location.hash = `#/runs/${root.id}`;
    render(App, { fetch: served, sockets: sockets.factory });
    const nav = await screen.findByRole('navigation', { name: 'Spawn tree' });
    const rootStream = stream(sockets, root.id);
    rootStream.open();
    rootStream.event(1, event(root.id, 1, log('root says hi')));
    expect(await screen.findByText('root says hi')).toBeInTheDocument();
    await user().click(await within(nav).findByRole('link', { name: /kid/ }));
    expect(window.location.hash).toBe(`#/runs/${kid.id}`);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    await vi.waitFor(() => stream(sockets, kid.id));
    const s = stream(sockets, kid.id);
    s.open();
    s.event(1, event(kid.id, 1, log('kid says hi')));
    expect(await screen.findByText('kid says hi')).toBeInTheDocument();
    expect(screen.queryByText('root says hi')).not.toBeInTheDocument();
    expect(rootStream.closed).toBe(true);
    // The tree socket stays: it is the same tree.
    expect(tree(sockets, root.id).closed).toBe(false);
    expect(within(nav).getByRole('link', { name: /kid/ })).toHaveAttribute('aria-current', 'page');
  });

  // 6
  it('sticks to the bottom until the user scrolls up, then offers to jump to the latest', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    const list = await screen.findByRole('list', { name: 'Events' });
    const box = list.parentElement as HTMLElement;
    Object.defineProperty(box, 'scrollHeight', { configurable: true, get: () => 1000 });
    Object.defineProperty(box, 'clientHeight', { configurable: true, get: () => 200 });
    const s = stream(sockets, r.id);
    s.open();
    s.event(1, event(r.id, 1, log('a')));
    await settle();
    expect(box.scrollTop).toBe(1000);
    box.scrollTop = 100;
    await fireEvent.scroll(box);
    s.event(2, event(r.id, 2, log('b')));
    await settle();
    expect(box.scrollTop).toBe(100);
    const jump = screen.getByRole('button', { name: 'Jump to latest' });
    await user().click(jump);
    expect(box.scrollTop).toBe(1000);
    expect(screen.queryByRole('button', { name: 'Jump to latest' })).not.toBeInTheDocument();
    Object.defineProperty(box, 'scrollHeight', { configurable: true, get: () => 1500 });
    s.event(3, event(r.id, 3, log('c')));
    await settle();
    expect(box.scrollTop).toBe(1500);
  });

  // 7
  it('renders a large tool result collapsed with its size, and expands it', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('list', { name: 'Events' });
    const s = stream(sockets, r.id);
    s.open();
    const output = 'x'.repeat(100_000);
    s.event(1, event(r.id, 1, { kind: 'tool_result', name: 'read_file', ok: true, output }));
    await settle();
    const toggle = screen.getByRole('button', { name: /read_file/ });
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
    expect(toggle).toHaveTextContent('100.0 kB');
    expect(document.body.textContent?.includes(output)).toBe(false);
    await user().click(toggle);
    expect(toggle).toHaveAttribute('aria-expanded', 'true');
    expect(document.body.textContent?.includes(output)).toBe(true);
  });

  // 8
  it('shows the budget bar in the warning style at 80%, with what is carved to children', async () => {
    const root = run({ spent: 1_600_000, limit: 2_000_000 });
    const kid = run({ parent: root.id, root: root.id, depth: 1, limit: 150_000 });
    show(root.id, [root, kid]);
    const meter = await screen.findByRole('meter', { name: 'Budget used' });
    expect(meter).toHaveAttribute('aria-valuenow', '80');
    expect(meter.className).toContain('warn');
    expect(screen.getByText('of $2.00 limit, 80% used')).toBeInTheDocument();
    expect(screen.getByText('Carved to children').nextElementSibling).toHaveTextContent('$0.15');
    expect(screen.getByText('Left to spend').nextElementSibling).toHaveTextContent('$0.40');
  });

  it('shows a run without a limit as spent only', async () => {
    const r = run({ spent: 250_000, limit: null });
    show(r.id, [r]);
    expect(await screen.findByText('$0.25', { selector: '.spent' })).toBeInTheDocument();
    expect(screen.getByText('no limit', { selector: 'p' })).toBeInTheDocument();
    expect(screen.queryByRole('meter')).not.toBeInTheDocument();
  });

  // 9
  it('enables only the controls the status allows', async () => {
    const button = (name: string) => screen.getByRole('button', { name });
    const r = run({ status: 'Running' });
    const { unmount } = show(r.id, [r]);
    await screen.findByRole('button', { name: 'Pause' });
    expect(button('Pause')).toBeEnabled();
    expect(button('Cancel run')).toBeEnabled();
    // Pause and Resume share one button: it offers only what the status allows.
    expect(screen.queryByRole('button', { name: 'Resume' })).not.toBeInTheDocument();
    unmount();
    const p = run({ status: { Suspended: 'Manual' } });
    const second = show(p.id, [p]);
    await screen.findByRole('button', { name: 'Resume' });
    expect(button('Resume')).toBeEnabled();
    expect(screen.queryByRole('button', { name: 'Pause' })).not.toBeInTheDocument();
    second.unmount();
    const a = run({ status: { Suspended: { Approval: 'ap-1' } } });
    const third = show(a.id, [a]);
    await screen.findByRole('button', { name: 'Pause' });
    expect(button('Pause')).toBeDisabled();
    expect(button('Cancel run')).toBeEnabled();
    third.unmount();
    const c = run({ status: 'Completed' });
    show(c.id, [c]);
    await screen.findByRole('button', { name: 'Pause' });
    for (const name of ['Pause', 'Cancel run', 'Set']) expect(button(name)).toBeDisabled();
    // An ended run takes no message: the box is gone, and the panel says why (issue #95).
    expect(screen.queryByRole('button', { name: 'Send' })).not.toBeInTheDocument();
  });

  it('turns Pause into Resume on the same button when the run is paused, keeping focus on it', async () => {
    const r = run({ status: 'Running' });
    const { sockets, calls } = show(r.id, [r]);
    const pause = await screen.findByRole('button', { name: 'Pause' });
    pause.focus();
    await user().keyboard('{Enter}');
    expect(calls.filter((c) => c.method === 'POST').map((c) => c.path)).toEqual([`/api/runs/${r.id}/pause`]);
    const t = tree(sockets, r.id);
    t.open();
    t.event(1, event(r.id, 1, { kind: 'status_changed', from: 'Running', to: { Suspended: 'Manual' } }));
    await settle();
    expect(pause).toHaveAccessibleName('Resume');
    expect(document.activeElement).toBe(pause);
    await user().keyboard('{Enter}');
    expect(calls.filter((c) => c.method === 'POST').map((c) => c.path).at(-1)).toBe(`/api/runs/${r.id}/resume`);
  });

  it('names what each status allows', () => {
    const live: RunStatus[] = ['Queued', 'Provisioning', 'Running'];
    for (const s of live) expect(allowed(s).cancel).toBe(true);
    expect(allowed('Running')).toEqual({ pause: true, resume: false, cancel: true, budget: true });
    expect(allowed({ Suspended: 'Budget' }).resume).toBe(true);
    expect(allowed({ Suspended: { Error: 'x' } }).resume).toBe(true);
    expect(allowed({ Suspended: { Approval: 'a' } }).resume).toBe(false);
    expect(allowed({ Failed: 'x' })).toEqual({ pause: false, resume: false, cancel: false, budget: false });
  });

  // PR #108 review: an ended run that still has live runs below it can be cancelled (the engine
  // cancels that tree); nothing else is allowed on it.
  it('allows only Cancel on an ended run with live runs below it', () => {
    for (const s of ['Completed', 'Cancelled', { Failed: 'x' }] as RunStatus[]) {
      expect(allowed(s, 2)).toEqual({ pause: false, resume: false, cancel: true, budget: false });
      expect(allowed(s, 0).cancel).toBe(false);
    }
    expect(allowed('Running', 0).cancel).toBe(true);
    expect(allowed('Queued', 3)).toEqual(allowed('Queued'));
  });

  it('offers Cancel on an ended root whose kept children still run, naming them', async () => {
    const root = run({ agent: 'lead', status: 'Completed' });
    const kid = run({ parent: root.id, root: root.id, depth: 1 });
    const { calls, sockets } = show(root.id, [root, kid]);
    const button = (name: string) => screen.getByRole('button', { name });
    const cancel = await screen.findByRole('button', { name: 'Cancel run' });
    expect(cancel).toBeEnabled();
    for (const name of ['Pause', 'Set']) expect(button(name)).toBeDisabled();
    expect(screen.queryByRole('button', { name: 'Send' })).not.toBeInTheDocument();
    const u = user();
    await u.click(cancel);
    expect(screen.getByRole('dialog')).toHaveTextContent('lead has ended. This cancels its 1 child run.');
    await u.click(screen.getByRole('button', { name: 'Yes, cancel it' }));
    expect(calls.filter((c) => c.method === 'POST').map((c) => c.path)).toEqual([`/api/runs/${root.id}/cancel`]);

    // Once the last child has ended there is nothing left to cancel.
    const t = tree(sockets, root.id);
    t.open();
    t.event(1, event(kid.id, 1, { kind: 'status_changed', from: 'Running', to: 'Cancelled' }));
    await settle();
    expect(button('Cancel run')).toBeDisabled();
  });

  // 10
  it('asks before cancelling, naming the live child runs; Esc keeps the run', async () => {
    const root = run({ agent: 'lead' });
    const kids = [
      run({ parent: root.id, root: root.id, depth: 1 }),
      run({ parent: root.id, root: root.id, depth: 1, status: 'Completed' }),
    ];
    const grand = [
      run({ parent: kids[0]?.id ?? '', root: root.id, depth: 2 }),
      run({ parent: kids[1]?.id ?? '', root: root.id, depth: 2, status: { Suspended: 'Manual' } }),
    ];
    const { calls } = show(root.id, [root, ...kids, ...grand]);
    const cancel = await screen.findByRole('button', { name: 'Cancel run' });
    const u = user();
    await u.click(cancel);
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveTextContent('and 3 child runs');
    await u.keyboard('{Escape}');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(document.activeElement).toBe(cancel);
    expect(calls.filter((c) => c.method === 'POST')).toEqual([]);
    await u.click(cancel);
    await u.click(screen.getByRole('button', { name: 'Yes, cancel it' }));
    expect(calls.filter((c) => c.method === 'POST').map((c) => c.path)).toEqual([`/api/runs/${root.id}/cancel`]);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(document.activeElement).toBe(cancel);
  });

  it('keeps focus inside the cancel dialog', async () => {
    const r = run();
    show(r.id, [r]);
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Cancel run' }));
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveTextContent('Cancel this run?');
    const keep = within(dialog).getByRole('button', { name: 'Keep running' });
    const confirm = within(dialog).getByRole('button', { name: 'Yes, cancel it' });
    expect(document.activeElement).toBe(keep);
    await u.tab();
    expect(document.activeElement).toBe(confirm);
    await u.tab();
    expect(document.activeElement).toBe(keep);
    await u.tab({ shift: true });
    expect(document.activeElement).toBe(confirm);
    await u.click(keep);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  // 11
  it('validates a new budget and posts it as a decimal string', async () => {
    const r = run();
    const { calls } = show(r.id, [r]);
    const input = await screen.findByLabelText('Change limit (USD)');
    const u = user();
    for (const bad of ['abc', '-1', '1.2345678']) {
      await u.clear(input);
      await u.type(input, bad);
      await u.click(screen.getByRole('button', { name: 'Set' }));
      expect(screen.getByRole('alert')).toHaveTextContent('Enter an amount in dollars');
    }
    expect(calls.filter((c) => c.method === 'POST')).toEqual([]);
    await u.clear(input);
    await u.type(input, '3.50');
    await u.click(screen.getByRole('button', { name: 'Set' }));
    const post = calls.find((c) => c.method === 'POST');
    expect(post).toEqual({ method: 'POST', path: `/api/runs/${r.id}/budget`, body: { usd: '3.50' } });
    expect(parseUsd('3.50')).toBe(3_500_000);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('sizes output in bytes, kB and MB', () => {
    expect(formatSize(512)).toBe('512 B');
    expect(formatSize(100_000)).toBe('100.0 kB');
    expect(formatSize(2_500_000)).toBe('2.5 MB');
  });

  it('counts live runs below a run and what it carved to its children', () => {
    const nodes = [
      { id: 'r', parent: null, status: 'Running' as RunStatus, limit: 10 },
      { id: 'a', parent: 'r', status: 'Running' as RunStatus, limit: 3 },
      { id: 'b', parent: 'r', status: 'Completed' as RunStatus, limit: null },
      { id: 'c', parent: 'b', status: 'Queued' as RunStatus, limit: 1 },
    ];
    expect(liveBelow(nodes, 'r')).toBe(2);
    expect(liveBelow(nodes, 'c')).toBe(0);
    expect(carved(nodes, 'r')).toBe(3);
  });

  it('adds the tokens of only the new events to running totals, without changing them (PR #85 review)', () => {
    const call = (tokens: number): Event['kind'] => ({
      kind: 'model_call', run_id: 'r', provider: 'p', model: 'm', tokens_in: tokens, tokens_out: tokens * 2,
      tokens_cache_read: tokens * 3, tokens_cache_write: tokens * 4, cost: null, latency_ms: 1, status: 200,
    });
    const before = { input: 10, output: 20, cacheRead: 30, cacheWrite: 40 };
    const after = addTokens(before, [event('r', 1, call(1)), event('r', 2, log('not a call')), event('r', 3, call(5))]);
    expect(after).toEqual({ input: 16, output: 32, cacheRead: 48, cacheWrite: 64 });
    expect(before).toEqual({ input: 10, output: 20, cacheRead: 30, cacheWrite: 40 });
    expect(addTokens(after, [])).toEqual(after);
  });

  it('parses dollars to micro-dollars without float math', () => {
    expect(parseUsd('0')).toBe(0);
    expect(parseUsd('2')).toBe(2_000_000);
    expect(parseUsd('0.000001')).toBe(1);
    expect(parseUsd(' 1.5 ')).toBe(1_500_000);
    for (const bad of ['', '.5', '1.', '1e3', '-1', 'abc', '1.2345678', '1,50']) expect(parseUsd(bad)).toBeNull();
  });

  // 12
  it('shows the server reason for a refused command and refreshes the controls', async () => {
    const r = run({ status: 'Running' });
    const runs = [r];
    const { calls } = show(r.id, runs, { status: 409, error: 'this run is not running' });
    const pause = await screen.findByRole('button', { name: 'Pause' });
    runs[0] = { ...r, status: 'Completed' };
    await user().click(pause);
    expect(await screen.findByText('this run is not running')).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Pause' })).toBeDisabled());
    expect(calls.filter((c) => c.method === 'GET' && c.path === `/api/runs/${r.id}`).length).toBeGreaterThanOrEqual(2);
  });

  it('sends a message, and shows why the server refused it', async () => {
    const r = run();
    const { calls } = show(r.id, [r], { status: 409, error: "this run's runtime does not take injected messages" });
    const u = user();
    await u.type(await screen.findByLabelText('Message'), 'please hurry');
    await u.click(screen.getByRole('button', { name: 'Send' }));
    expect(calls.find((c) => c.method === 'POST')).toEqual({
      method: 'POST',
      path: `/api/runs/${r.id}/message`,
      body: { text: 'please hurry' },
    });
    expect(await screen.findByText("this run's runtime does not take injected messages")).toBeInTheDocument();
  });

  it('posts pause and resume, and clears an old error on success', async () => {
    const r = run({ status: { Suspended: 'Manual' } });
    const { calls } = show(r.id, [r]);
    await user().click(await screen.findByRole('button', { name: 'Resume' }));
    expect(calls.filter((c) => c.method === 'POST').map((c) => c.path)).toEqual([`/api/runs/${r.id}/resume`]);
  });

  // 13
  it('says when the run does not exist, with a link to Fleet', async () => {
    show('missing', []);
    expect(await screen.findByText('Run not found.')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Back to Fleet' })).toHaveAttribute('href', '#/fleet');
  });

  it('shows a load error as an alert, and Retry loads again', async () => {
    const sockets = fakeSockets();
    const r = run({ agent: 'lead' });
    let fails = 1;
    const fetch: typeof globalThis.fetch = async (input) => {
      if (fails-- > 0) return new Response(JSON.stringify({ error: 'database is locked' }), { status: 500 });
      const url = new URL(String(input), 'http://kestrel.test');
      return new Response(JSON.stringify(url.pathname === '/api/runs' ? [r] : r));
    };
    render(Run, { id: r.id, api: createApi({ fetch, getToken: () => null }), onUnauthorized: () => {}, sockets: sockets.factory, base: 'ws://kestrel.test' });
    expect(await screen.findByRole('alert')).toHaveTextContent('database is locked');
    await user().click(screen.getByRole('button', { name: 'Retry' }));
    expect(await screen.findByRole('heading', { level: 1, name: 'lead' })).toBeInTheDocument();
  });

  // 14
  it('shows a failed child\'s reason on its tree node and in its stream', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1, status: { Failed: 'sandbox exited with code 1' } });
    const { sockets } = show(kid.id, [root, kid]);
    const nav = await screen.findByRole('navigation', { name: 'Spawn tree' });
    const node = await within(nav).findByRole('link', { name: /kid/ });
    expect(node).toHaveAccessibleDescription(/sandbox exited with code 1/);
    expect(node).toHaveAttribute('title', 'Failed: sandbox exited with code 1');
    const s = stream(sockets, kid.id);
    s.open();
    s.event(1, event(kid.id, 1, { kind: 'status_changed', from: 'Running', to: { Failed: 'sandbox exited with code 1' } }));
    await settle();
    expect(items().at(-1)).toContain('Failed: sandbox exited with code 1');
  });

  // 15
  it('has no serious or critical axe violations, with the dialog open too', async () => {
    const root = run({ agent: 'lead', spent: 1_700_000 });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1, status: { Failed: 'boom' } });
    const { container, sockets } = show(root.id, [root, kid]);
    await screen.findByRole('navigation', { name: 'Spawn tree' });
    const s = stream(sockets, root.id);
    s.open();
    s.event(1, event(root.id, 1, { kind: 'tool_call', name: 'shell', input: { cmd: 'ls' } }));
    s.event(2, event(root.id, 2, { kind: 'tool_result', name: 'shell', ok: false, output: 'denied' }));
    await settle();
    vi.useRealTimers();
    expect(ids(await seriousViolations(container))).toEqual([]);
    await userEvent.click(screen.getByRole('button', { name: 'Cancel run' }));
    expect(ids(await seriousViolations(container))).toEqual([]);
  });

  it('is operable from the keyboard: tree toggles, event toggles and controls', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const { sockets } = show(root.id, [root, kid]);
    await screen.findByRole('link', { name: /kid/ });
    const s = stream(sockets, root.id);
    s.open();
    s.event(1, event(root.id, 1, { kind: 'tool_call', name: 'shell', input: { cmd: 'ls' } }));
    await settle();
    const u = user();
    const reach = async (el: HTMLElement) => {
      for (let i = 0; i < 40 && document.activeElement !== el; i++) await u.tab();
      expect(document.activeElement).toBe(el);
    };
    const pause = screen.getByRole('button', { name: 'Pause' });
    await reach(pause);
    await u.keyboard('{Enter}');
    await vi.waitFor(() => expect(document.activeElement).toBe(pause));
    const toggle = screen.getByRole('button', { name: 'Children of lead' });
    await reach(toggle);
    await u.keyboard('{Enter}');
    expect(screen.queryByRole('link', { name: /kid/ })).not.toBeInTheDocument();
    await u.keyboard(' ');
    expect(screen.getByRole('link', { name: /kid/ })).toBeInTheDocument();
    const call = screen.getByRole('button', { name: /shell/ });
    await reach(call);
    await u.keyboard('{Enter}');
    expect(call).toHaveAttribute('aria-expanded', 'true');
    expect(screen.getByText(/"cmd": "ls"/)).toBeInTheDocument();
  });

  it('renders every kind of event', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('list', { name: 'Events' });
    const s = stream(sockets, r.id);
    s.open();
    const kinds: Event['kind'][] = [
      { kind: 'run_created', agent: 'worker' },
      { kind: 'sandbox_provisioned', driver: 'gvisor', sandbox_id: 'sb-1' },
      { kind: 'agent_message', text: 'Looking at the tests.' },
      {
        kind: 'model_call', run_id: r.id, provider: 'anthropic', model: 'claude-x', tokens_in: 1200, tokens_out: 300,
        tokens_cache_read: 0, tokens_cache_write: 0, cost: 4_200, latency_ms: 900, status: 200,
      },
      {
        kind: 'model_call', run_id: r.id, provider: 'local', model: 'free', tokens_in: 1, tokens_out: 1,
        tokens_cache_read: 0, tokens_cache_write: 0, cost: null, latency_ms: 1, status: 200,
      },
      { kind: 'budget_warning', spent: 1_600_000, limit: 2_000_000 },
      { kind: 'approval_requested', id: 'ap-1', action: 'push to main' },
      { kind: 'approval_resolved', id: 'ap-1', approved: true },
      { kind: 'approval_resolved', id: 'ap-2', approved: false },
      { kind: 'child_spawned', child: 'kid-9', agent: 'helper', budget: 500_000 },
      { kind: 'capabilities_dropped', tools: ['shell'], hosts: ['example.com'], secrets: [], agents: [] },
      { kind: 'run_finished', summary: 'All green.' },
      { kind: 'run_finished', summary: null },
      { kind: 'status_changed', from: 'Running', to: 'Completed' },
    ];
    kinds.forEach((k, i) => s.event(i + 1, event(r.id, i + 1, k)));
    await settle();
    const text = items();
    expect(text).toHaveLength(kinds.length);
    expect(text[0]).toContain('Run created');
    expect(text[1]).toContain('Sandbox ready: gvisor');
    expect(text[2]).toContain('Looking at the tests.');
    expect(text[3]).toContain('claude-x');
    expect(text[3]).toContain('1200 in, 300 out');
    expect(text[3]).toContain('$0.0042');
    expect(text[4]).toContain('cost not known');
    expect(text[5]).toContain('Budget warning: $1.60 of $2.00');
    expect(text[6]).toContain('Approval requested: push to main');
    expect(screen.getByRole('link', { name: 'Open approvals' })).toHaveAttribute('href', '#/approvals');
    expect(text[7]).toContain('Approved');
    expect(text[8]).toContain('Rejected');
    expect(text[9]).toContain('Spawned helper with $0.50');
    expect(screen.getByRole('link', { name: 'helper' })).toHaveAttribute('href', '#/runs/kid-9');
    expect(text[10]).toContain('Capabilities dropped: shell, example.com');
    expect(text[11]).toContain('Finished: All green.');
    expect(text[12]).toContain('Finished');
    expect(text[13]).toContain('Running to Completed');
  });
});

describe('Run view: moving between runs, the dialog and budgets', () => {
  it('clears the controls when another run is shown', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const { rerender } = show(kid.id, [root, kid], { status: 409, error: 'nope' });
    const u = user();
    await u.type(await screen.findByLabelText('Message'), 'hi');
    await u.click(screen.getByRole('button', { name: 'Send' }));
    expect(await screen.findByText('nope')).toBeInTheDocument();
    await u.type(screen.getByLabelText('Change limit (USD)'), 'abc');
    await rerender({ id: root.id });
    await screen.findByRole('heading', { level: 1, name: 'lead' });
    expect(screen.queryByText('nope')).not.toBeInTheDocument();
    expect(screen.getByLabelText('Change limit (USD)')).toHaveValue('');
  });

  it('closes an open cancel dialog when another run is shown, so it never cancels the wrong run', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const { rerender, calls } = show(kid.id, [root, kid]);
    await user().click(await screen.findByRole('button', { name: 'Cancel run' }));
    expect(screen.getByRole('dialog')).toHaveTextContent('This cancels kid.');
    await rerender({ id: root.id });
    await screen.findByRole('heading', { level: 1, name: 'lead' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(calls.filter((c) => c.method === 'POST')).toEqual([]);
  });

  it('a run that is not found after another was shown drops the old run and its sockets', async () => {
    const root = run({ agent: 'lead' });
    const { rerender, sockets } = show(root.id, [root]);
    await screen.findByRole('heading', { level: 1, name: 'lead' });
    await rerender({ id: 'nope' });
    expect(await screen.findByText('Run not found.')).toBeInTheDocument();
    expect(screen.getByRole('heading', { level: 1, name: 'Run nope' })).toBeInTheDocument();
    expect(stream(sockets, root.id).closed).toBe(true);
    expect(tree(sockets, root.id).closed).toBe(true);
  });

  it('moving to another tree closes the old sockets and opens new ones; unmount closes all', async () => {
    const a = run({ agent: 'alpha' });
    const b = run({ agent: 'beta' });
    const { rerender, sockets, unmount } = show(a.id, [a, b]);
    await screen.findByRole('heading', { level: 1, name: 'alpha' });
    await rerender({ id: b.id });
    await screen.findByRole('heading', { level: 1, name: 'beta' });
    expect(stream(sockets, a.id).closed).toBe(true);
    expect(tree(sockets, a.id).closed).toBe(true);
    expect(stream(sockets, b.id).closed).toBe(false);
    expect(tree(sockets, b.id).closed).toBe(false);
    unmount();
    expect(sockets.made.every((x) => x.closed)).toBe(true);
  });

  it('shows a run of the same tree that the tree has not delivered yet', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const sockets = fakeSockets();
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = new URL(String(input), 'http://kestrel.test');
      if (url.pathname === '/api/runs') return new Response(JSON.stringify([root]));
      const one = [root, kid].find((r) => url.pathname === `/api/runs/${r.id}`);
      return new Response(JSON.stringify(one));
    };
    const { rerender } = render(Run, {
      id: root.id,
      api: createApi({ fetch, getToken: () => null }),
      onUnauthorized: () => {},
      sockets: sockets.factory,
      base: 'ws://kestrel.test',
    });
    await screen.findByRole('heading', { level: 1, name: 'lead' });
    await rerender({ id: kid.id });
    expect(await screen.findByRole('heading', { level: 1, name: 'kid' })).toBeInTheDocument();
  });

  it('fetches the run again after a new budget, so the bar shows it', async () => {
    const r = run({ limit: 2_000_000 });
    const runs = [r];
    show(r.id, runs);
    const u = user();
    await u.type(await screen.findByLabelText('Change limit (USD)'), '3.50');
    runs[0] = { ...r, limit: 3_500_000 };
    await u.click(screen.getByRole('button', { name: 'Set' }));
    await vi.advanceTimersByTimeAsync(2_000);
    expect(screen.getByText('of $3.50 limit, 0% used')).toBeInTheDocument();
    expect(screen.getByText('Left to spend').nextElementSibling).toHaveTextContent('$3.50');
    // The box is ready for the next amount.
    expect(screen.getByLabelText('Change limit (USD)')).toHaveValue('');
  });

  it('keeps focus in the dialog after a click on its text or the backdrop; Esc still closes it', async () => {
    const r = run();
    show(r.id, [r]);
    const u = user();
    const cancel = await screen.findByRole('button', { name: 'Cancel run' });
    await u.click(cancel);
    const dialog = screen.getByRole('dialog');
    await u.click(within(dialog).getByText('Cancel this run?'));
    await u.tab({ shift: true });
    expect(dialog.contains(document.activeElement)).toBe(true);
    await u.click(dialog.parentElement as HTMLElement);
    await u.tab();
    expect(dialog.contains(document.activeElement)).toBe(true);
    // Focus moved behind the dialog by other means (a script, assistive tech) comes back into it.
    screen.getByRole('button', { name: 'Pause' }).focus();
    expect(dialog.contains(document.activeElement)).toBe(true);
    await u.click(dialog.parentElement as HTMLElement);
    await u.keyboard('{Escape}');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(document.activeElement).toBe(cancel);
  });

  it('closes the dialog when the run ends while it is open, keeping focus in the controls', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await user().click(await screen.findByRole('button', { name: 'Cancel run' }));
    const t = tree(sockets, r.id);
    t.open();
    t.event(1, event(r.id, 1, { kind: 'status_changed', from: 'Running', to: 'Completed' }));
    await settle();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(document.activeElement).toBe(screen.getByRole('group', { name: 'Controls' }));
  });

  it('sends a command once however fast it is clicked', async () => {
    const r = run();
    const { calls } = show(r.id, [r]);
    const pause = await screen.findByRole('button', { name: 'Pause' });
    // Both clicks land before the first command is answered.
    void fireEvent.click(pause);
    void fireEvent.click(pause);
    await settle();
    expect(calls.filter((c) => c.method === 'POST')).toHaveLength(1);
  });
});

/** A plain fetch for App-level tests: runs by id and the list. */
function serveFetch(runs: RunSummary[]): typeof globalThis.fetch {
  return async (input) => {
    const url = new URL(String(input), 'http://kestrel.test');
    if (url.pathname === '/api/runs') {
      const root = url.searchParams.get('root');
      return new Response(JSON.stringify(runs.filter((r) => root === null || r.root === root)));
    }
    const one = runs.find((r) => url.pathname === `/api/runs/${r.id}`);
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no"}', { status: 404 });
  };
}

// Issue #95: the Message box is shown only for a run that can take a message now, and says why
// otherwise. A message is recorded as an event, and so is its delivery to the agent.
describe('Run view: messages to the agent', () => {
  it('says why a run cannot take a message', () => {
    expect(messageRefusal('Running', true)).toBeNull();
    const notGranted = messageRefusal('Running', false) ?? '';
    expect(notGranted).toContain('does not take messages');
    expect(notGranted).toContain('kestrel.messages');
    expect(messageRefusal('Queued', true)).toBe('Only a running run takes messages.');
    expect(messageRefusal({ Suspended: 'Manual' }, true)).toBe('Only a running run takes messages.');
    expect(messageRefusal('Completed', true)).toBe('The run has ended, so it takes no more messages.');
    expect(messageRefusal({ Failed: 'x' }, false)).toBe('The run has ended, so it takes no more messages.');
    // An agent that never takes them says so, whatever its status.
    expect(messageRefusal('Queued', false)).toBe(notGranted);
  });

  it('hides the Message box for an agent that does not take messages, and says why', async () => {
    const r = run({ takes_messages: false });
    show(r.id, [r]);
    await screen.findByRole('heading', { level: 1, name: 'worker' });
    const panel = await screen.findByRole('region', { name: 'Message to the agent' });
    expect(within(panel).queryByRole('textbox')).not.toBeInTheDocument();
    expect(within(panel).queryByRole('button', { name: 'Send' })).not.toBeInTheDocument();
    expect(panel).toHaveTextContent('does not take messages');
  });

  it('hides the Message box while the run is not running, and shows it once it runs', async () => {
    const r = run({ status: 'Queued' });
    const { sockets } = show(r.id, [r]);
    const panel = await screen.findByRole('region', { name: 'Message to the agent' });
    expect(within(panel).queryByRole('textbox')).not.toBeInTheDocument();
    expect(panel).toHaveTextContent('Only a running run takes messages.');
    const t = tree(sockets, r.id);
    t.open();
    t.event(1, event(r.id, 1, { kind: 'status_changed', from: 'Queued', to: 'Running' }));
    await settle();
    expect(await screen.findByRole('textbox', { name: 'Message' })).toBeEnabled();
    expect(panel).not.toHaveTextContent('Only a running run');
  });

  it('says a sent message is on its way, and clears the box', async () => {
    const r = run();
    const { calls } = show(r.id, [r]);
    const u = user();
    const box = await screen.findByRole('textbox', { name: 'Message' });
    await u.type(box, 'also fix the docs');
    await u.click(screen.getByRole('button', { name: 'Send' }));
    await vi.waitFor(() => expect(box).toHaveValue(''));
    expect(calls.filter((c) => c.method === 'POST')).toEqual([
      { method: 'POST', path: `/api/runs/${r.id}/message`, body: { text: 'also fix the docs' } },
    ]);
    const panel = screen.getByRole('region', { name: 'Message to the agent' });
    expect(within(panel).getByRole('status')).toHaveTextContent('Sent. The agent reads it the next time it checks for messages.');
  });

  // Review of #95: the server counts UTF-8 bytes, and the box's maxlength counts characters, so
  // the page counts bytes itself before sending.
  it('refuses a message over 16 KiB in UTF-8 bytes without sending it, and sends one at 16 KiB', async () => {
    const r = run();
    const { calls } = show(r.id, [r]);
    const u = user();
    const box = await screen.findByRole('textbox', { name: 'Message' });
    const panel = screen.getByRole('region', { name: 'Message to the agent' });
    const posts = () => calls.filter((c) => c.method === 'POST');
    // 8193 two-byte characters: fewer than 16384 characters, but 16386 bytes.
    const over = 'é'.repeat(8193);
    await fireEvent.input(box, { target: { value: over } });
    await u.click(screen.getByRole('button', { name: 'Send' }));
    const alert = within(panel).getByRole('alert');
    expect(alert).toHaveTextContent('16386 bytes');
    expect(alert).toHaveTextContent('16384');
    expect(posts()).toEqual([]);
    expect(box).toHaveValue(over);
    // Exactly 16 KiB goes, and the warning goes with it.
    const fits = 'é'.repeat(8192);
    await fireEvent.input(box, { target: { value: fits } });
    await u.click(screen.getByRole('button', { name: 'Send' }));
    await vi.waitFor(() => expect(posts()).toHaveLength(1));
    expect(posts()[0]?.body).toEqual({ text: fits });
    expect(within(panel).queryByRole('alert')).not.toBeInTheDocument();
  });

  it('shows a message and its delivery in the events', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('list', { name: 'Events' });
    const s = stream(sockets, r.id);
    s.open();
    s.event(1, event(r.id, 1, { kind: 'message_sent', text: 'also fix the docs' }));
    s.event(2, event(r.id, 2, { kind: 'message_delivered', seqs: [1] }));
    s.event(3, event(r.id, 3, { kind: 'message_delivered', seqs: [4, 5] }));
    await settle();
    const text = items();
    expect(text[0]).toContain('Message to the agent: also fix the docs');
    expect(text[1]).toContain('The agent read 1 message');
    expect(text[1]).not.toContain('messages');
    expect(text[2]).toContain('The agent read 2 messages');
  });
});
