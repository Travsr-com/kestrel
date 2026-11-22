// Issue #84, scenarios 10 to 13 and 17: the Run view restyled to the mockup. Component tests
// against a fake API and fake sockets, like `run.test.ts`.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import Run from '../src/views/Run.svelte';
import { createApi } from '../src/lib/api';
import type { Event, ModelCallFields, RunSummary } from '../src/lib/types';
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
    created_at: new Date(NOW - 60_000 + n).toISOString(),
    ...over,
  };
}

interface Call {
  method: string;
  path: string;
  body: unknown;
}

function show(id: string, runs: RunSummary[], refuse: string | null = null) {
  const calls: Call[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = new URL(String(input), 'http://kestrel.test');
    const method = init?.method ?? 'GET';
    calls.push({ method, path: url.pathname, body: typeof init?.body === 'string' ? JSON.parse(init.body) : null });
    if (method === 'GET' && url.pathname === '/api/runs') {
      const root = url.searchParams.get('root');
      return new Response(JSON.stringify(runs.filter((r) => root === null || r.root === root)));
    }
    const one = runs.find((r) => url.pathname.startsWith(`/api/runs/${r.id}`));
    if (!one) return new Response(JSON.stringify({ error: 'run not found' }), { status: 404 });
    if (method === 'GET') return new Response(JSON.stringify(one));
    if (refuse !== null) return new Response(JSON.stringify({ error: refuse }), { status: 409 });
    return new Response(null, { status: 202 });
  };
  const sockets = fakeSockets();
  const result = render(Run, {
    id,
    api: createApi({ fetch, getToken: () => null }),
    onUnauthorized: () => {},
    sockets: sockets.factory,
    base: 'ws://kestrel.test',
  });
  return { ...result, sockets, calls };
}

function event(runId: string, seq: number, kind: Event['kind']): Event {
  return { run_id: runId, seq, ts: new Date(NOW).toISOString(), kind };
}

function socket(sockets: ReturnType<typeof fakeSockets>, part: string): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes(part)).at(-1);
  if (!found) throw new Error(`no socket for ${part}`);
  return found;
}

function modelCall(runId: string, over: Partial<ModelCallFields> = {}): Event['kind'] {
  return {
    kind: 'model_call',
    run_id: runId,
    provider: 'anthropic',
    model: 'claude-x',
    tokens_in: 0,
    tokens_out: 0,
    tokens_cache_read: 0,
    tokens_cache_write: 0,
    cost: 1_000,
    latency_ms: 10,
    status: 200,
    ...over,
  };
}

/** The value a definition list gives a term. */
function definition(term: string): string {
  const dt = screen.getByText(term, { selector: 'dt' });
  return dt.nextElementSibling?.textContent?.trim() ?? '';
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

describe('Run view header (scenario 10)', () => {
  it('shows a breadcrumb back to Fleet, the agent with a status pill, and the run facts', async () => {
    const root = run({ agent: 'migrate-orm', id: '00000000-0000-4000-8000-0000aa01fd41' });
    const kid = run({ agent: 'schema-reader', parent: root.id, root: root.id, depth: 1, sandbox_tier: 'docker-runc' });
    show(kid.id, [root, kid]);
    const heading = await screen.findByRole('heading', { level: 1, name: 'schema-reader' });
    const crumbs = screen.getByRole('navigation', { name: 'Breadcrumb' });
    expect(within(crumbs).getByRole('link', { name: 'Fleet' })).toHaveAttribute('href', '#/fleet');
    expect(crumbs).toHaveTextContent(/^Fleet\s*\/\s*schema-reader$/);
    // The last item is the page itself.
    expect(within(crumbs).getByText('schema-reader')).toHaveAttribute('aria-current', 'page');
    expect(within(crumbs).getByRole('link', { name: 'Fleet' })).not.toHaveAttribute('aria-current');
    const header = heading.closest('header') as HTMLElement;
    const pill = within(header).getByText('Running', { selector: '.pill' });
    expect(pill).toHaveClass('tone-running');
    expect(within(header).getByText(`run ${kid.id.slice(-8)}`)).toBeInTheDocument();
    expect(within(header).getByText('docker-runc')).toBeInTheDocument();
    expect(within(header).getByRole('link', { name: 'Parent: migrate-orm' })).toHaveAttribute('href', `#/runs/${root.id}`);
  });

  it('pills a run waiting on an approval as Needs approval, and shows why a run failed', async () => {
    const r = run({ agent: 'fixer', status: { Suspended: { Approval: 'ap-1' } } });
    const { unmount } = show(r.id, [r]);
    const header = (await screen.findByRole('heading', { level: 1, name: 'fixer' })).closest('header') as HTMLElement;
    expect(within(header).getByText('Needs approval', { selector: '.pill' })).toHaveClass('tone-approval');
    unmount();
    const f = run({ agent: 'broken', status: { Failed: 'sandbox exited with code 1' } });
    show(f.id, [f]);
    const failed = (await screen.findByRole('heading', { level: 1, name: 'broken' })).closest('header') as HTMLElement;
    expect(within(failed).getByText('Failed', { selector: '.pill' })).toHaveClass('tone-failed');
    expect(within(failed).getByText('sandbox exited with code 1')).toBeInTheDocument();
  });

  it('puts Pause and Cancel run in the header, and Resume instead of Pause for a paused run', async () => {
    const r = run({ agent: 'lead' });
    const { unmount } = show(r.id, [r]);
    const header = (await screen.findByRole('heading', { level: 1, name: 'lead' })).closest('header') as HTMLElement;
    expect(within(header).getByRole('button', { name: 'Pause' })).toBeEnabled();
    expect(within(header).queryByRole('button', { name: 'Resume' })).not.toBeInTheDocument();
    const cancel = within(header).getByRole('button', { name: 'Cancel run' });
    expect(cancel).toHaveClass('danger');
    unmount();
    const p = run({ agent: 'idle', status: { Suspended: 'Manual' } });
    show(p.id, [p]);
    const paused = (await screen.findByRole('heading', { level: 1, name: 'idle' })).closest('header') as HTMLElement;
    expect(within(paused).getByRole('button', { name: 'Resume' })).toBeEnabled();
    expect(within(paused).queryByRole('button', { name: 'Pause' })).not.toBeInTheDocument();
  });

  it('asks before cancelling, naming the live children, with Keep running and Yes, cancel it', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ parent: root.id, root: root.id, depth: 1 });
    const { calls } = show(root.id, [root, kid]);
    const u = user();
    await u.click(await screen.findByRole('button', { name: 'Cancel run' }));
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveTextContent('This cancels lead and 1 child run.');
    expect(within(dialog).getByRole('button', { name: 'Keep running' })).toBeInTheDocument();
    // Never two buttons with the same name.
    expect(screen.getAllByRole('button', { name: 'Cancel run' })).toHaveLength(1);
    await u.click(within(dialog).getByRole('button', { name: 'Yes, cancel it' }));
    expect(calls.filter((c) => c.method === 'POST').map((c) => c.path)).toEqual([`/api/runs/${root.id}/cancel`]);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });
});

describe('Run view panels (scenario 11)', () => {
  it('shows a Spawn tree panel whose rows carry an icon, the status pill text and the cost', async () => {
    const root = run({ agent: 'lead', spent: 6_800_000 });
    const done = run({ agent: 'reader', parent: root.id, root: root.id, depth: 1, status: 'Completed', spent: 640_000 });
    const failed = run({ agent: 'broken', parent: root.id, root: root.id, depth: 1, status: { Failed: 'boom' } });
    show(root.id, [root, done, failed]);
    const panel = (await screen.findByRole('heading', { level: 2, name: 'Spawn tree' })).closest('section') as HTMLElement;
    expect(panel).toHaveClass('panel');
    const nav = within(panel).getByRole('navigation', { name: 'Spawn tree' });
    const row = (name: string) => within(nav).getByRole('link', { name }).closest('.row') as HTMLElement;
    await within(nav).findByRole('link', { name: 'reader' });
    expect(row('lead')).toHaveClass('selected');
    expect(row('reader')).not.toHaveClass('selected');
    expect(within(row('lead')).getByText('Running')).toHaveClass('tone-running');
    expect(within(row('lead')).getByText('$6.80')).toBeInTheDocument();
    expect(within(row('reader')).getByText('Completed')).toHaveClass('tone-done');
    expect(within(row('reader')).getByText('$0.64')).toBeInTheDocument();
    expect(within(row('broken')).getByText('Failed')).toHaveClass('tone-failed');
    const icon = (name: string) => row(name).querySelector('.icon');
    expect(icon('lead')).toHaveAttribute('aria-hidden', 'true');
    expect(icon('lead')).toHaveAttribute('data-icon', 'play');
    expect(icon('reader')).toHaveAttribute('data-icon', 'check');
    expect(icon('broken')).toHaveAttribute('data-icon', 'cross');
  });

  it('picks the icon for each status', async () => {
    const root = run({ agent: 'lead' });
    const kids = [
      run({ agent: 'asks', status: { Suspended: { Approval: 'ap-1' } } }),
      run({ agent: 'paused', status: { Suspended: 'Manual' } }),
      run({ agent: 'broke', status: { Suspended: 'Budget' } }),
      run({ agent: 'waiting', status: 'Queued' }),
    ].map((k) => ({ ...k, parent: root.id, root: root.id, depth: 1 }));
    show(root.id, [root, ...kids]);
    const nav = await screen.findByRole('navigation', { name: 'Spawn tree' });
    await within(nav).findByRole('link', { name: 'waiting' });
    const icon = (name: string) =>
      within(nav).getByRole('link', { name }).closest('.row')?.querySelector('.icon')?.getAttribute('data-icon');
    expect(icon('asks')).toBe('pause');
    expect(icon('paused')).toBe('pause');
    expect(icon('broke')).toBe('clock');
    expect(icon('waiting')).toBe('clock');
  });

  it('shows a Live events panel that says Connecting until the sockets open, then Connected, or Reconnecting while one is down', async () => {
    const r = run({ agent: 'lead' });
    const { sockets } = show(r.id, [r]);
    const panel = (await screen.findByRole('heading', { level: 2, name: 'Live events' })).closest('section') as HTMLElement;
    expect(panel).toHaveClass('panel');
    expect(within(panel).getByRole('region', { name: 'Event stream' })).toBeInTheDocument();
    expect(within(panel).getByRole('list', { name: 'Events' })).toBeInTheDocument();
    const word = () => panel.querySelector('.link') as HTMLElement;
    expect(word()).toHaveTextContent('Connecting');
    const s = socket(sockets, `/ws/runs/${r.id}`);
    s.open();
    await settle();
    // The tree socket has not opened yet.
    expect(word()).toHaveTextContent('Connecting');
    socket(sockets, `/ws/tree/${r.id}`).open();
    await vi.waitFor(() => expect(word()).toHaveTextContent('Connected'));
    s.drop();
    await vi.waitFor(() => expect(word()).toHaveTextContent('Reconnecting'));
    await vi.advanceTimersByTimeAsync(600);
    socket(sockets, `/ws/runs/${r.id}`).open();
    await vi.waitFor(() => expect(word()).toHaveTextContent('Connected'));
  });

  it('the visible word is not a live region, so opening or switching runs announces nothing (PR #85 review)', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const { sockets, rerender } = show(root.id, [root, kid]);
    const panel = (await screen.findByRole('heading', { level: 2, name: 'Live events' })).closest('section') as HTMLElement;
    const word = panel.querySelector('.link') as HTMLElement;
    expect(word).toHaveTextContent('Connecting');
    // Neither the word nor anything around it is a live region.
    expect(word.closest('[role="status"], [role="alert"], [aria-live]')).toBeNull();
    const live = within(panel).getByRole('status');
    expect(live).toHaveClass('visually-hidden');
    expect(live.textContent).toBe('');
    socket(sockets, `/ws/runs/${root.id}`).open();
    socket(sockets, `/ws/tree/${root.id}`).open();
    await vi.waitFor(() => expect(word).toHaveTextContent('Connected'));
    expect(live.textContent).toBe('');
    await rerender({ id: kid.id });
    await screen.findByRole('heading', { level: 1, name: 'kid' });
    expect(panel.querySelector('.link')).toHaveTextContent('Connecting');
    expect(within(panel).getByRole('status').textContent).toBe('');
    socket(sockets, `/ws/runs/${kid.id}`).open();
    // New props re-run the effects that read them, so the tree socket may be a new one too.
    socket(sockets, `/ws/tree/${root.id}`).open();
    await vi.waitFor(() => expect(panel.querySelector('.link')).toHaveTextContent('Connected'));
    expect(within(panel).getByRole('status').textContent).toBe('');
  });

  it('announces when a socket drops and when live updates come back, once each (PR #85 review)', async () => {
    const r = run({ agent: 'lead' });
    const { sockets } = show(r.id, [r]);
    const panel = (await screen.findByRole('heading', { level: 2, name: 'Live events' })).closest('section') as HTMLElement;
    const live = () => within(panel).getByRole('status');
    const s = socket(sockets, `/ws/runs/${r.id}`);
    s.open();
    socket(sockets, `/ws/tree/${r.id}`).open();
    await vi.waitFor(() => expect(panel.querySelector('.link')).toHaveTextContent('Connected'));
    expect(live().textContent).toBe('');
    s.drop();
    await vi.waitFor(() => expect(live()).toHaveTextContent('Reconnecting to live updates.'));
    await vi.advanceTimersByTimeAsync(600);
    socket(sockets, `/ws/runs/${r.id}`).open();
    await vi.waitFor(() => expect(live()).toHaveTextContent('Live updates are back.'));
    expect(panel.querySelector('.link')).toHaveTextContent('Connected');
  });

  it('shows each event with its time, a short coloured kind label and its text', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('list', { name: 'Events' });
    const s = socket(sockets, `/ws/runs/${r.id}`);
    s.open();
    const cases: [Event['kind'], string, string, string][] = [
      [{ kind: 'run_created', agent: 'worker' }, 'created', 'muted', 'Run created: worker'],
      [{ kind: 'status_changed', from: 'Queued', to: 'Running' }, 'status', 'muted', 'Status changed'],
      [{ kind: 'sandbox_provisioned', driver: 'gvisor', sandbox_id: 'sb' }, 'sandbox', 'muted', 'Sandbox ready'],
      [{ kind: 'agent_message', text: 'hello' }, 'message', 'muted', 'hello'],
      [{ kind: 'tool_call', name: 'shell', input: {} }, 'tool', 'running', 'Tool call: shell'],
      [{ kind: 'tool_result', name: 'shell', ok: true, output: 'ok' }, 'result', 'running', 'Tool result: shell, ok'],
      [{ kind: 'tool_result', name: 'shell', ok: false, output: 'no' }, 'result', 'danger', 'Tool result: shell, failed'],
      [modelCall(r.id, { tokens_in: 3210, tokens_out: 410 }), 'model', 'accent', 'claude-x'],
      [{ kind: 'budget_warning', spent: 1, limit: 2 }, 'budget', 'approval', 'Budget warning'],
      [{ kind: 'approval_requested', id: 'a', action: 'push' }, 'approval', 'approval', 'Approval requested: push'],
      [{ kind: 'approval_resolved', id: 'a', approved: true }, 'approval', 'approval', 'Approved'],
      [{ kind: 'agent_log', line: 'npm test' }, 'log', 'muted', 'npm test'],
      [{ kind: 'run_finished', summary: 'All green.' }, 'done', 'done', 'Finished: All green.'],
      [{ kind: 'child_spawned', child: 'kid', agent: 'helper', budget: 2_000_000 }, 'spawn', 'suspended', 'Spawned helper'],
      [{ kind: 'capabilities_dropped', tools: ['shell'], hosts: [], secrets: [], agents: [] }, 'dropped', 'muted', 'shell'],
    ];
    cases.forEach(([kind], i) => s.event(i + 1, event(r.id, i + 1, kind)));
    await settle();
    const rows = within(screen.getByRole('list', { name: 'Events' })).getAllByRole('listitem');
    expect(rows).toHaveLength(cases.length);
    const time = new Date(NOW).toLocaleTimeString([], { hour12: false });
    cases.forEach(([, label, colour, text], i) => {
      const row = rows[i] as HTMLElement;
      expect(row.querySelector('time')).toHaveTextContent(time);
      const kind = row.querySelector('.kind');
      expect(kind).toHaveTextContent(new RegExp(`^${label}$`));
      expect(kind).toHaveClass(`c-${colour}`);
      expect(row.querySelector('.text')).toHaveTextContent(text);
    });
  });
});

describe('Run view budget panel (scenario 12)', () => {
  it('shows the spend, the limit and percent used, a meter, and what is carved and left', async () => {
    const root = run({ spent: 6_800_000, limit: 10_000_000 });
    const kid = run({ parent: root.id, root: root.id, depth: 1, limit: 2_000_000 });
    show(root.id, [root, kid]);
    const panel = (await screen.findByRole('heading', { level: 2, name: 'Budget' })).closest('section') as HTMLElement;
    expect(panel).toHaveClass('panel');
    expect(within(panel).getByText('$6.80', { selector: '.spent' })).toBeInTheDocument();
    expect(within(panel).getByText('of $10.00 limit, 68% used')).toBeInTheDocument();
    const meter = within(panel).getByRole('meter', { name: 'Budget used' });
    expect(meter).toHaveClass('track');
    expect(meter).toHaveAttribute('aria-valuenow', '68');
    expect(meter).toHaveAttribute('aria-valuetext', '68%');
    expect(definition('Carved to children')).toBe('$2.00');
    expect(definition('Left to spend')).toBe('$3.20');
  });

  it('sums the tokens of the model calls in the run stream, live', async () => {
    const r = run();
    const { sockets } = show(r.id, [r]);
    await screen.findByRole('heading', { level: 2, name: 'Budget' });
    for (const term of ['Input tokens', 'Output tokens', 'Cache reads', 'Cache writes']) expect(definition(term)).toBe('0');
    const s = socket(sockets, `/ws/runs/${r.id}`);
    s.open();
    s.event(1, event(r.id, 1, modelCall(r.id, { tokens_in: 400_000, tokens_out: 58_000, tokens_cache_read: 1_200_000, tokens_cache_write: 96_000 })));
    s.event(2, event(r.id, 2, { kind: 'agent_log', line: 'not a model call' }));
    await settle();
    expect(definition('Input tokens')).toBe('400,000');
    s.event(3, event(r.id, 3, modelCall(r.id, { tokens_in: 12_880, tokens_out: 204, tokens_cache_read: 4_115, tokens_cache_write: 330 })));
    await settle();
    expect(definition('Input tokens')).toBe('412,880');
    expect(definition('Output tokens')).toBe('58,204');
    expect(definition('Cache reads')).toBe('1,204,115');
    expect(definition('Cache writes')).toBe('96,330');
  });

  it('starts the token totals again from zero when another run is shown (PR #85 review)', async () => {
    const root = run({ agent: 'lead' });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const { sockets, rerender } = show(root.id, [root, kid]);
    await screen.findByRole('heading', { level: 2, name: 'Budget' });
    const s = socket(sockets, `/ws/runs/${root.id}`);
    s.open();
    s.event(1, event(root.id, 1, modelCall(root.id, { tokens_in: 500, tokens_out: 50, tokens_cache_read: 5, tokens_cache_write: 1 })));
    await settle();
    expect(definition('Input tokens')).toBe('500');
    await rerender({ id: kid.id });
    await screen.findByRole('heading', { level: 1, name: 'kid' });
    for (const term of ['Input tokens', 'Output tokens', 'Cache reads', 'Cache writes']) expect(definition(term)).toBe('0');
    const k = socket(sockets, `/ws/runs/${kid.id}`);
    k.open();
    k.event(1, event(kid.id, 1, modelCall(kid.id, { tokens_in: 7, tokens_out: 3 })));
    await settle();
    expect(definition('Input tokens')).toBe('7');
    expect(definition('Output tokens')).toBe('3');
  });

  it('says no limit, with no meter, for a run without a limit', async () => {
    const r = run({ spent: 250_000, limit: null });
    show(r.id, [r]);
    const panel = (await screen.findByRole('heading', { level: 2, name: 'Budget' })).closest('section') as HTMLElement;
    expect(within(panel).getByText('$0.25', { selector: '.spent' })).toBeInTheDocument();
    expect(within(panel).getByText('no limit', { selector: 'p' })).toBeInTheDocument();
    expect(within(panel).queryByRole('meter')).not.toBeInTheDocument();
    expect(definition('Left to spend')).toBe('no limit');
  });
});

describe('Run view change limit (scenario 13)', () => {
  it('refuses a bad amount with the same message, and sends a good one and clears the field', async () => {
    const r = run();
    const { calls } = show(r.id, [r]);
    const input = await screen.findByLabelText('Change limit (USD)');
    expect(input).toHaveAccessibleDescription('Sub-agents draw from this limit. Their total can never go above it.');
    const set = screen.getByRole('button', { name: 'Set' });
    expect(set).toHaveClass('primary');
    const u = user();
    await u.type(input, '1.2345678');
    await u.click(set);
    expect(screen.getByRole('alert')).toHaveTextContent(
      'Enter an amount in dollars, like 2.50, with at most 6 decimal places.',
    );
    expect(calls.filter((c) => c.method === 'POST')).toEqual([]);
    await u.clear(input);
    await u.type(input, '3.50');
    await u.click(set);
    expect(calls.find((c) => c.method === 'POST')).toEqual({ method: 'POST', path: `/api/runs/${r.id}/budget`, body: { usd: '3.50' } });
    expect(input).toHaveValue('');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('shows why the server refused a new limit in the Change limit panel, and keeps the amount', async () => {
    const r = run();
    const { calls } = show(r.id, [r], 'the limit is below what is already spent');
    const input = await screen.findByLabelText('Change limit (USD)');
    const u = user();
    await u.type(input, '0.10');
    await u.click(screen.getByRole('button', { name: 'Set' }));
    const form = input.closest('form') as HTMLElement;
    expect(await within(form).findByRole('alert')).toHaveTextContent('the limit is below what is already spent');
    expect(input).toHaveValue('0.10');
    // The run is fetched again, so the controls match the server.
    await vi.waitFor(() =>
      expect(calls.filter((c) => c.method === 'GET' && c.path === `/api/runs/${r.id}`).length).toBeGreaterThanOrEqual(2),
    );
  });

  it('keeps the Message panel: label, input and Send', async () => {
    const r = run();
    const { calls } = show(r.id, [r]);
    const u = user();
    await u.type(await screen.findByLabelText('Message'), 'hurry');
    await u.click(screen.getByRole('button', { name: 'Send' }));
    expect(calls.find((c) => c.method === 'POST')?.path).toBe(`/api/runs/${r.id}/message`);
  });
});

describe('Run view accessibility (scenario 17)', () => {
  it('has no serious or critical axe violations with every panel filled and the dialog open', async () => {
    const root = run({ agent: 'lead', spent: 1_700_000 });
    const kid = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1, status: { Failed: 'boom' } });
    const { container, sockets } = show(root.id, [root, kid]);
    await screen.findByRole('navigation', { name: 'Spawn tree' });
    const s = socket(sockets, `/ws/runs/${root.id}`);
    s.open();
    s.event(1, event(root.id, 1, modelCall(root.id, { tokens_in: 10 })));
    s.event(2, event(root.id, 2, { kind: 'child_spawned', child: kid.id, agent: 'kid', budget: 100 }));
    s.event(3, event(root.id, 3, { kind: 'approval_requested', id: 'a', action: 'push' }));
    await settle();
    vi.useRealTimers();
    expect(ids(await seriousViolations(container))).toEqual([]);
    await userEvent.click(screen.getByRole('button', { name: 'Cancel run' }));
    expect(ids(await seriousViolations(container))).toEqual([]);
  });
});

describe('Run view controls listeners (PR #85 review)', () => {
  it('listens for keys and focus on the window once, in the part that owns the cancel dialog, until unmounted', async () => {
    const added = vi.spyOn(window, 'addEventListener');
    const removed = vi.spyOn(window, 'removeEventListener');
    const count = (spy: typeof added, type: string) => spy.mock.calls.filter(([t]) => t === type).length;
    try {
      const r = run({ agent: 'lead' });
      const { unmount } = show(r.id, [r]);
      await screen.findByRole('heading', { level: 1, name: 'lead' });
      await screen.findByLabelText('Message');
      expect(count(added, 'keydown')).toBe(1);
      expect(count(added, 'focusin')).toBe(1);
      // The dialog still closes on Escape.
      const u = user();
      await u.click(screen.getByRole('button', { name: 'Cancel run' }));
      await u.keyboard('{Escape}');
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
      unmount();
      expect(count(removed, 'keydown')).toBe(1);
      expect(count(removed, 'focusin')).toBe(1);
    } finally {
      added.mockRestore();
      removed.mockRestore();
    }
  });
});
