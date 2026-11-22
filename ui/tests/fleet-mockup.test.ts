// Issue #84: the Fleet view restyled to the mockup. Scenarios 4 to 8, and 17 for this view.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import Fleet from '../src/views/Fleet.svelte';
import { createApi } from '../src/lib/api';
import { ApprovalsState } from '../src/lib/approvals.svelte';
import type { Approval, Event, RunSummary } from '../src/lib/types';
import { fakeSockets } from './fake-socket';
import { ids, seriousViolations } from './axe';

const NOW = Date.parse('2026-11-21T10:00:00Z');

let seq = 0;
function run(over: Partial<RunSummary> = {}): RunSummary {
  seq += 1;
  const id = over.id ?? `00000000-0000-4000-8000-${String(seq).padStart(12, '0')}`;
  return {
    id,
    agent: 'worker',
    status: 'Running',
    parent: null,
    root: id,
    depth: 0,
    spent: 420_000,
    limit: 2_000_000,
    sandbox_tier: 'gvisor',
    takes_messages: true,
    created_at: new Date(NOW - 125_000).toISOString(),
    ...over,
  };
}

function approval(runId: string, id: string, action: string): Approval {
  return { id, run_id: runId, action, status: 'pending', requested_at: new Date(NOW).toISOString(), question: false };
}

/** Pending approvals as App.svelte holds them. */
function pending(...list: Approval[]): ApprovalsState {
  const state = new ApprovalsState();
  state.load(list, state.beginSync());
  return state;
}

function show(runs: RunSummary[], approvals?: ApprovalsState) {
  const fetch: typeof globalThis.fetch = async (input) => {
    const url = new URL(String(input), 'http://kestrel.test');
    if (url.pathname === '/api/runs') return new Response(JSON.stringify(runs));
    const one = runs.find((r) => url.pathname === `/api/runs/${r.id}`);
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
  };
  const sockets = fakeSockets();
  const result = render(Fleet, {
    api: createApi({ fetch, getToken: () => null }),
    onUnauthorized: () => {},
    sockets: sockets.factory,
    base: 'ws://kestrel.test',
    ...(approvals ? { approvals } : {}),
  });
  return { ...result, sockets };
}

function event(runId: string, kind: Event['kind'], s = 1): Event {
  return { run_id: runId, seq: s, ts: new Date(NOW).toISOString(), kind };
}

const cards = () => screen.queryAllByRole('article');

function card(agent: string): HTMLElement {
  const found = cards().find((c) => within(c).queryByRole('link', { name: agent }));
  if (!found) throw new Error(`no card for ${agent}`);
  return found;
}

/** The tile whose title is `title`. */
function tile(title: string): HTMLElement {
  const found = screen.getByText(title, { selector: 'dt' }).parentElement;
  if (!found) throw new Error(`no tile ${title}`);
  return found;
}

const chip = (name: string) => screen.getByRole('button', { name: new RegExp(`^${name} \\d+$`) });
const chipCount = (name: string) => chip(name).textContent?.replace(name, '').trim();

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

const user = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime });

describe('Fleet header', () => {
  it('shows the title, the line under it and a labelled search', async () => {
    show([run({ agent: 'alpha' })]);
    await screen.findByRole('link', { name: 'alpha' });
    expect(screen.getByRole('heading', { level: 1, name: 'Fleet' })).toBeInTheDocument();
    expect(screen.getByText('Every active run on this machine, live.')).toBeInTheDocument();
    const search = screen.getByRole('searchbox', { name: 'Search runs and agents' });
    expect(search).toHaveAttribute('placeholder', 'Search runs and agents');
  });
});

describe('Fleet tiles (scenario 4)', () => {
  function fleet() {
    const lead = run({ agent: 'lead', spent: 1_000_000 });
    return [
      lead,
      run({ agent: 'broke', status: { Suspended: 'Budget' }, spent: 2_000_000 }),
      run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } }, spent: 500_000 }),
      run({ agent: 'kid', parent: lead.id, root: lead.id, depth: 1, spent: 250_000 }),
      run({ agent: 'done', status: 'Completed', spent: 5_000_000 }),
    ];
  }

  it('counts active runs, their spend, what needs you and live sub-agents', async () => {
    const runs = fleet();
    show(runs, pending(approval(runs[2]?.id ?? '', 'ap-1', 'git push'), approval('elsewhere', 'ap-2', 'rm -rf')));
    await screen.findByRole('link', { name: 'lead' });
    expect(tile('Active runs')).toHaveTextContent('4');
    expect(tile('Active runs')).toHaveTextContent('1 suspended on budget');
    expect(tile('Spend')).toHaveTextContent('$3.75');
    expect(tile('Spend')).toHaveTextContent('across 4 active runs');
    expect(tile('Needs you')).toHaveTextContent('2');
    expect(tile('Needs you')).toHaveTextContent('approvals waiting');
    expect(tile('Sub-agents')).toHaveTextContent('1');
    expect(tile('Sub-agents')).toHaveTextContent('live child runs');
  });

  it('Needs you says it is loading, not 0, until the approvals list has loaded (PR #85 review)', async () => {
    const runs = fleet();
    const state = new ApprovalsState();
    show(runs, state);
    await screen.findByRole('link', { name: 'lead' });
    const big = tile('Needs you').querySelector('.big') as HTMLElement;
    expect(big).not.toHaveTextContent('0');
    expect(within(big).getByText('...')).toHaveAttribute('aria-hidden', 'true');
    expect(within(big).getByText('loading')).toHaveClass('visually-hidden');
    state.load([approval(runs[2]?.id ?? '', 'ap-1', 'git push')], state.beginSync());
    await vi.waitFor(() => expect(big).toHaveTextContent(/^1$/));
  });

  it('Needs you says the approvals could not be loaded when their request fails, not loading (PR #85 review)', async () => {
    const runs = fleet();
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = new URL(String(input), 'http://kestrel.test');
      if (url.pathname === '/api/approvals') return new Response('{"error":"database is locked"}', { status: 500 });
      if (url.pathname === '/api/runs') return new Response(JSON.stringify(url.searchParams.has('status') ? [] : runs));
      const one = runs.find((r) => url.pathname === `/api/runs/${r.id}`);
      return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
    };
    window.location.hash = '#/fleet';
    render(App, { fetch, sockets: fakeSockets().factory });
    await screen.findByRole('link', { name: 'lead' });
    const big = tile('Needs you').querySelector('.big') as HTMLElement;
    await vi.waitFor(() => expect(within(big).getByText('?')).toHaveAttribute('aria-hidden', 'true'));
    expect(within(big).getByText('approvals could not be loaded')).toHaveClass('visually-hidden');
    expect(big).not.toHaveTextContent('loading');
  });

  it('without the approvals list, counts the runs waiting on an approval', async () => {
    show(fleet());
    await screen.findByRole('link', { name: 'lead' });
    expect(within(tile('Needs you')).getByText('1')).toBeInTheDocument();
  });

  it('the big numbers are in the mono font', async () => {
    show(fleet());
    await screen.findByRole('link', { name: 'lead' });
    expect(within(tile('Active runs')).getByText('4')).toHaveClass('mono');
  });
});

describe('Fleet chips (scenario 5)', () => {
  function fleet() {
    return [
      run({ agent: 'go', status: 'Running' }),
      run({ agent: 'booting', status: 'Provisioning' }),
      run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } } }),
      run({ agent: 'broke', status: { Suspended: 'Budget' } }),
      run({ agent: 'held', status: { Suspended: 'Manual' } }),
      run({ agent: 'stuck', status: { Suspended: { Error: 'disk full' } } }),
      run({ agent: 'waiting', status: 'Queued' }),
      run({ agent: 'done', status: 'Completed' }),
      run({ agent: 'broken', status: { Failed: 'exit 1' } }),
    ];
  }

  it('each chip shows its count, and All is selected at first', async () => {
    show(fleet());
    await screen.findByRole('link', { name: 'go' });
    expect(chipCount('All')).toBe('7');
    expect(chipCount('Running')).toBe('2');
    expect(chipCount('Needs you')).toBe('1');
    expect(chipCount('Suspended')).toBe('3');
    expect(chipCount('Finished')).toBe('2');
    expect(chip('All')).toHaveAttribute('aria-pressed', 'true');
    expect(chip('Running')).toHaveAttribute('aria-pressed', 'false');
    expect(cards()).toHaveLength(7);
  });

  it('selecting a chip shows only its runs and marks it pressed', async () => {
    show(fleet());
    await screen.findByRole('link', { name: 'go' });
    const u = user();
    const agents = () => cards().map((c) => within(c).getAllByRole('link')[0]?.textContent).sort();

    await u.click(chip('Running'));
    expect(chip('Running')).toHaveAttribute('aria-pressed', 'true');
    expect(chip('All')).toHaveAttribute('aria-pressed', 'false');
    expect(agents()).toEqual(['booting', 'go']);

    await u.click(chip('Needs you'));
    expect(agents()).toEqual(['asking']);

    await u.click(chip('Suspended'));
    expect(agents()).toEqual(['broke', 'held', 'stuck']);

    await u.click(chip('Finished'));
    expect(agents()).toEqual(['broken', 'done']);

    await u.click(chip('All'));
    expect(cards()).toHaveLength(7);
  });

  it('Finished shows a run that ended in view, however long ago', async () => {
    const a = run({ agent: 'alpha' });
    const { sockets } = show([a, run({ agent: 'beta' })]);
    await screen.findByRole('link', { name: 'alpha' });
    const socket = sockets.made.find((s) => s.url.includes(a.id));
    if (!socket) throw new Error('no socket');
    socket.open();
    socket.event(1, event(a.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }, 1));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(screen.queryByRole('link', { name: 'alpha' })).not.toBeInTheDocument();
    await user().click(chip('Finished'));
    expect(screen.getByRole('link', { name: 'alpha' })).toBeInTheDocument();
  });

  it('counts follow Roots only but not the selected chip', async () => {
    const lead = run({ agent: 'lead' });
    show([lead, run({ agent: 'kid', parent: lead.id, root: lead.id, depth: 1 })]);
    await screen.findByRole('link', { name: 'lead' });
    const u = user();
    expect(chipCount('Running')).toBe('1');
    await u.click(chip('Finished'));
    expect(chipCount('Running')).toBe('1');
    await u.click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(chipCount('Running')).toBe('2');
    expect(chipCount('All')).toBe('2');
  });

  it('keeps Roots only at the end of the chip row', async () => {
    show([run({ agent: 'alpha' })]);
    await screen.findByRole('link', { name: 'alpha' });
    const row = screen.getByRole('group', { name: 'Filters' });
    const controls = [...row.querySelectorAll('button, input')];
    expect(controls.map((c) => c.getAttribute('type'))).toEqual(['button', 'button', 'button', 'button', 'button', 'checkbox']);
    expect(controls.at(-1)).toBe(within(row).getByRole('checkbox', { name: 'Roots only' }));
  });

  it('choosing a chip goes back to the first page', async () => {
    show(Array.from({ length: 60 }, (_, i) => run({ agent: `run-${i}` })));
    await screen.findAllByRole('article');
    const u = user();
    await u.click(screen.getByRole('button', { name: 'Next' }));
    expect(screen.getByText('Showing 51 to 60 of 60')).toBeInTheDocument();
    await u.click(chip('Running'));
    expect(screen.getByText('Showing 1 to 50 of 60')).toBeInTheDocument();
  });
});

describe('Fleet search (scenario 6)', () => {
  it('keeps runs whose agent contains the text, in any case', async () => {
    show([run({ agent: 'pr-reviewer' }), run({ agent: 'docs-sync' }), run({ agent: 'PR-merger' })]);
    await screen.findByRole('link', { name: 'docs-sync' });
    await user().type(screen.getByRole('searchbox', { name: 'Search runs and agents' }), 'pR-');
    expect(cards().map((c) => within(c).getAllByRole('link')[0]?.textContent).sort()).toEqual(['PR-merger', 'pr-reviewer']);
  });

  it('keeps runs whose full id contains the text', async () => {
    const a = run({ id: 'run-ABCDEF-0001', agent: 'alpha' });
    show([a, run({ id: 'run-zzzzzz-0002', agent: 'beta' })]);
    await screen.findByRole('link', { name: 'alpha' });
    await user().type(screen.getByRole('searchbox', { name: 'Search runs and agents' }), 'abcdef');
    expect(cards()).toHaveLength(1);
    expect(card('alpha')).toBeInTheDocument();
  });

  it('says when nothing matches, and the chip counts follow the search', async () => {
    show([run({ agent: 'alpha' }), run({ agent: 'beta' })]);
    await screen.findByRole('link', { name: 'alpha' });
    const box = screen.getByRole('searchbox', { name: 'Search runs and agents' });
    const u = user();
    await u.type(box, 'alp');
    expect(chipCount('All')).toBe('1');
    await u.type(box, 'zzz');
    expect(screen.getByText('No run matches these filters.')).toBeInTheDocument();
    expect(chipCount('All')).toBe('0');
    await u.clear(box);
    expect(cards()).toHaveLength(2);
  });

  it('typing goes back to the first page', async () => {
    show(Array.from({ length: 60 }, (_, i) => run({ agent: `run-${i}` })));
    await screen.findAllByRole('article');
    const u = user();
    await u.click(screen.getByRole('button', { name: 'Next' }));
    await u.type(screen.getByRole('searchbox', { name: 'Search runs and agents' }), 'run');
    expect(screen.getByText('Showing 1 to 50 of 60')).toBeInTheDocument();
  });
});

describe('Fleet card (scenario 7)', () => {
  it('shows the agent, the short id, a pill, the spend with a percent and a meter, and a footer', async () => {
    const a = run({ id: '00000000-0000-4000-8000-0000a1b2c3d4', agent: 'alpha', spent: 420_000, limit: 2_000_000 });
    show([a]);
    await screen.findByRole('link', { name: 'alpha' });
    const c = card('alpha');
    expect(c).toHaveClass('panel', 'tone-running');
    expect(within(c).getByRole('heading', { level: 2, name: 'alpha' })).toBeInTheDocument();
    expect(within(c).getByText('run a1b2c3d4')).toHaveClass('mono');
    const pill = within(c).getByText('Running');
    expect(pill).toHaveClass('pill', 'tone-running');
    expect(within(c).getByText('$0.42 of $2.00')).toBeInTheDocument();
    expect(within(c).getByText('21%')).toBeInTheDocument();
    // Named by agent and run, so a list of cards does not read as many meters of the same name.
    const meter = within(c).getByRole('meter', { name: 'Budget used by alpha, run a1b2c3d4' });
    expect(meter).toHaveAttribute('aria-valuenow', '21');
    expect(meter).toHaveClass('track');
    expect(c).toHaveTextContent('no sub-agents');
    expect(c).toHaveTextContent('2m 05s');
    expect(c).toHaveTextContent('gvisor');
  });

  it('counts sub-agents in the footer, and those running', async () => {
    const lead = run({ agent: 'lead' });
    const solo = run({ agent: 'solo' });
    show([
      lead,
      solo,
      run({ agent: 'k1', parent: lead.id, root: lead.id, depth: 1 }),
      run({ agent: 'k2', parent: lead.id, root: lead.id, depth: 1 }),
      run({ agent: 'k3', parent: lead.id, root: lead.id, depth: 1, status: 'Queued' }),
      run({ agent: 'k4', parent: solo.id, root: solo.id, depth: 1, status: 'Queued' }),
    ]);
    await screen.findByRole('link', { name: 'lead' });
    expect(card('lead')).toHaveTextContent('3 sub-agents, 2 running');
    expect(card('solo')).toHaveTextContent('1 sub-agent');
    expect(card('solo')).not.toHaveTextContent('running');
  });

  it('cards of the same agent have meters of different names (PR #85 review)', async () => {
    const a = run({ id: '00000000-0000-4000-8000-00000000aaaa', agent: 'twin' });
    const b = run({ id: '00000000-0000-4000-8000-00000000bbbb', agent: 'twin' });
    show([a, b]);
    await screen.findAllByRole('link', { name: 'twin' });
    const names = screen.getAllByRole('meter').map((m) => m.getAttribute('aria-label')).sort();
    expect(names).toEqual(['Budget used by twin, run 0000aaaa', 'Budget used by twin, run 0000bbbb']);
  });

  it('a run with no limit says so and shows no meter', async () => {
    show([run({ agent: 'free', limit: null })]);
    await screen.findByRole('link', { name: 'free' });
    expect(card('free')).toHaveTextContent('no limit');
    expect(within(card('free')).queryByRole('meter')).not.toBeInTheDocument();
  });

  it('a spent budget fills the meter to 100%', async () => {
    show([run({ agent: 'broke', status: { Suspended: 'Budget' }, spent: 5_000_000, limit: 5_000_000 })]);
    await screen.findByRole('link', { name: 'broke' });
    expect(within(card('broke')).getByRole('meter')).toHaveAttribute('aria-valuenow', '100');
    expect(card('broke')).toHaveTextContent('100%');
    expect(card('broke')).toHaveClass('tone-suspended');
  });

  it('cards sit in a list, so a screen reader can count them', async () => {
    show([run({ agent: 'alpha' }), run({ agent: 'beta' })]);
    await screen.findByRole('link', { name: 'alpha' });
    const list = screen.getByRole('list');
    expect(within(list).getAllByRole('article')).toHaveLength(2);
    expect(list).toHaveClass('cards');
  });
});

describe('Fleet card status line (scenario 8)', () => {
  it('a run waiting on an approval says what it asks, with an approval border', async () => {
    const a = run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } } });
    show([a], pending(approval(a.id, 'ap-1', 'npm install')));
    await screen.findByRole('link', { name: 'asking' });
    expect(card('asking')).toHaveTextContent('npm install');
    expect(card('asking')).toHaveClass('tone-approval');
    expect(within(card('asking')).getByText('Needs approval')).toBeInTheDocument();
  });

  it('a long ask shows only its first line, capped (PR #85 review)', async () => {
    const a = run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } } });
    show([a], pending(approval(a.id, 'ap-1', `${'x'.repeat(30_000)}\n${'y'.repeat(20_000)}`)));
    await screen.findByRole('link', { name: 'asking' });
    expect(card('asking').querySelector('.note')?.textContent).toBe(`${'x'.repeat(120)}...`);
  });

  it('says what the approval the run waits on asks, picked by its id, when the run has two (PR #85 review)', async () => {
    const a = run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } } });
    show([a], pending(approval(a.id, 'ap-1', 'npm install'), approval(a.id, 'ap-2', 'git push')));
    await screen.findByRole('link', { name: 'asking' });
    expect(card('asking').querySelector('.note')?.textContent).toBe('npm install');
  });

  it('says it waits when the approval it waits on is not in the list, even with another of the run there (PR #85 review)', async () => {
    const a = run({ agent: 'asking', status: { Suspended: { Approval: 'ap-9' } } });
    show([a], pending(approval(a.id, 'ap-1', 'npm install')));
    await screen.findByRole('link', { name: 'asking' });
    expect(card('asking').querySelector('.note')?.textContent).toBe('Waiting for an approval.');
  });

  it('a run waiting on an approval not in the list says it waits', async () => {
    show([run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } } })]);
    await screen.findByRole('link', { name: 'asking' });
    expect(card('asking')).toHaveTextContent('Waiting for an approval.');
  });

  it('a run suspended on budget says how to continue', async () => {
    show([run({ agent: 'broke', status: { Suspended: 'Budget' } })]);
    await screen.findByRole('link', { name: 'broke' });
    expect(card('broke')).toHaveTextContent('Budget used up. Raise the limit to continue.');
    expect(within(card('broke')).getByText('Suspended')).toBeInTheDocument();
  });

  it('a paused run says it is paused', async () => {
    show([run({ agent: 'held', status: { Suspended: 'Manual' } })]);
    await screen.findByRole('link', { name: 'held' });
    expect(card('held')).toHaveTextContent('Paused.');
  });

  it('a failed run shows its reason', async () => {
    show([run({ agent: 'live' }), run({ agent: 'broken', status: { Failed: 'model refused the key' } })]);
    await screen.findByRole('link', { name: 'live' });
    await user().click(chip('Finished'));
    expect(card('broken')).toHaveTextContent('model refused the key');
    expect(within(card('broken')).getByText('Failed')).toBeInTheDocument();
  });

  it('a running run has no status line', async () => {
    show([run({ agent: 'go' })]);
    await screen.findByRole('link', { name: 'go' });
    expect(card('go').querySelector('.note')).toBeNull();
  });
});

describe('Fleet axe (scenario 17)', () => {
  it('has no serious or critical violations with tiles, chips, search and every card kind', async () => {
    const lead = run({ agent: 'lead' });
    const asking = run({ agent: 'asking', status: { Suspended: { Approval: 'ap-1' } } });
    const { container } = show(
      [
        lead,
        asking,
        run({ agent: 'kid', parent: lead.id, root: lead.id, depth: 1 }),
        run({ agent: 'broke', status: { Suspended: 'Budget' }, spent: 5_000_000, limit: 5_000_000 }),
        run({ agent: 'free', limit: null }),
      ],
      pending(approval(asking.id, 'ap-1', 'git push')),
    );
    await screen.findByRole('link', { name: 'lead' });
    vi.useRealTimers();
    expect(ids(await seriousViolations(container))).toEqual([]);
  });
});
