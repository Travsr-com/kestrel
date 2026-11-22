// Issue #84, scenarios 14 to 17 for Approvals: the pending list with a selected item, the detail
// panel of the selected approval, and the recently decided list. Component tests of the whole app
// against a fake API and fake sockets.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import type { Approval, ApprovalStatus, Event, RunSummary } from '../src/lib/types';
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
    spent: 420_000,
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

const ago = (ms: number) => new Date(NOW - ms).toISOString();

interface Call {
  method: string;
  path: string;
  search: string;
  body: unknown;
}

interface Options {
  /** Decided approvals; each `?status=` request gets those of its status. */
  decided?: Approval[];
  /** A decided status whose request fails. */
  failDecided?: ApprovalStatus;
  resolve?: () => Response | Promise<Response>;
}

/** A fake API: approvals by status, the runs list (the sidebar's count) and runs by id. */
function serve(pending: Approval[], runs: RunSummary[], options: Options = {}) {
  const calls: Call[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = new URL(String(input), 'http://kestrel.test');
    const method = init?.method ?? 'GET';
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : null;
    calls.push({ method, path: url.pathname, search: url.search, body });
    if (url.pathname === '/api/approvals') {
      const status = url.searchParams.get('status');
      if (status === 'pending') return new Response(JSON.stringify(pending));
      if (status === options.failDecided) return new Response('{"error":"database is locked"}', { status: 500 });
      return new Response(JSON.stringify((options.decided ?? []).filter((a) => a.status === status)));
    }
    if (url.pathname.startsWith('/api/approvals/')) return options.resolve?.() ?? new Response(null, { status: 202 });
    if (url.pathname === '/api/runs') return new Response(JSON.stringify(runs));
    const one = runs.find((r) => url.pathname === `/api/runs/${r.id}`);
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
  };
  return { fetch, calls };
}

function show(pending: Approval[], runs: RunSummary[], options?: Options) {
  const served = serve(pending, runs, options);
  const sockets = fakeSockets();
  window.location.hash = '#/approvals';
  const result = render(App, { fetch: served.fetch, sockets: sockets.factory });
  return { ...result, sockets, calls: served.calls };
}

function feed(sockets: ReturnType<typeof fakeSockets>): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes('/ws/events')).at(-1);
  if (!found) throw new Error('no events socket');
  return found;
}

function event(runId: string, kind: Event['kind'], at = NOW): Event {
  return { run_id: runId, seq: 1, ts: new Date(at).toISOString(), kind };
}

const pendingList = () => screen.getByRole('list', { name: 'Pending approvals' });
const items = () => within(pendingList()).queryAllByRole('listitem');
const decidedList = () => screen.getByRole('list', { name: 'Recently decided' });
/** The button that selects the pending item at `i`. */
const pick = (i: number) => within(items()[i] as HTMLElement).getByRole('button');
/** The detail panel of the selected approval. */
const detail = () => screen.getByRole('region', { name: /asks/ });
const text = (el: Element) => el.textContent?.replace(/\s+/g, ' ').trim() ?? '';
const posts = (calls: Call[]) => calls.filter((c) => c.method === 'POST');

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

const user = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime });

describe('Approvals: header and pending list (scenario 14)', () => {
  it('shows the heading and the line under it', async () => {
    show([], []);
    expect(await screen.findByRole('heading', { level: 1, name: 'Approvals' })).toHaveAttribute('tabindex', '-1');
    expect(screen.getByText('Agents stop and ask before risky steps. Nothing runs until you decide.')).toBeInTheDocument();
  });

  it('lists each pending approval with its agent, action and wait, newest first, and selects the first', async () => {
    const lead = run({ agent: 'lead' });
    const other = run({ agent: 'mailer' });
    show(
      [
        approval(lead, { action: 'git push origin main', requested_at: ago(180_000) }),
        approval(other, { action: 'Which branch?', question: true, requested_at: ago(45_000) }),
      ],
      [lead, other],
    );
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    expect(screen.getByText('Pending (2)')).toBeInTheDocument();
    await vi.waitFor(() => expect(items()[1]).toHaveTextContent('lead'));
    const [first, second] = items() as [HTMLElement, HTMLElement];
    expect(first).toHaveTextContent('mailer');
    expect(first).toHaveTextContent('Which branch?');
    expect(first).toHaveTextContent('Question, waiting 45s');
    expect(second).toHaveTextContent('git push origin main');
    expect(second).toHaveTextContent('Command, waiting 3m 00s');
    expect(second).toHaveTextContent('3 min');
    expect(pick(0)).toHaveAttribute('aria-pressed', 'true');
    expect(pick(1)).toHaveAttribute('aria-pressed', 'false');
    expect(within(detail()).getByText('Which branch?')).toBeInTheDocument();
  });

  it('says "agent not known yet" until the run is loaded', async () => {
    const r = run();
    show([approval(r)], []);
    await vi.waitFor(() => expect(items()).toHaveLength(1));
    expect(items()[0]).toHaveTextContent('agent not known yet');
  });

  it('selecting another item shows its detail', async () => {
    const lead = run({ agent: 'lead' });
    const kid = run({ agent: 'kid' });
    show(
      [approval(lead, { action: 'deploy', requested_at: ago(10_000) }), approval(kid, { action: 'rm -rf build', requested_at: ago(20_000) })],
      [lead, kid],
    );
    await vi.waitFor(() => expect(detail()).toHaveTextContent('lead'));
    await user().click(pick(1));
    expect(pick(1)).toHaveAttribute('aria-pressed', 'true');
    expect(pick(0)).toHaveAttribute('aria-pressed', 'false');
    expect(detail()).toHaveTextContent('rm -rf build');
    await vi.waitFor(() => expect(within(detail()).getByRole('heading', { level: 2 })).toHaveTextContent('kid asks for approval'));
  });

  it('a live-added approval goes on top without taking the selection', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'first', requested_at: ago(10_000) });
    const b = approval(r, { action: 'second', requested_at: ago(20_000) });
    const { sockets } = show([a, b], [r]);
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    await user().click(pick(1));
    const s = feed(sockets);
    s.open();
    s.event(9, event(r.id, { kind: 'approval_requested', id: 'new-1', action: 'deploy to staging' }, NOW + 1_000));
    await vi.waitFor(() => expect(items()).toHaveLength(3));
    expect(items()[0]).toHaveTextContent('deploy to staging');
    expect((items()[2] as HTMLElement).dataset.id).toBe(b.id);
    expect(pick(2)).toHaveAttribute('aria-pressed', 'true');
    expect(detail()).toHaveTextContent('second');
  });

  it('with nothing pending says so and shows no detail panel', async () => {
    show([], []);
    expect(await screen.findByText('Nothing waiting for you')).toBeInTheDocument();
    expect(screen.queryByRole('region', { name: /asks/ })).not.toBeInTheDocument();
  });
});

describe('Approvals: the detail panel (scenario 15)', () => {
  it('shows the kind, who asks, the run, the wait, the budget, the action and the buttons in order', async () => {
    const lead = run({ agent: 'lead', spent: 3_050_000, limit: 4_000_000 });
    const a = approval(lead, { action: 'npm install --save-dev jest-retry', requested_at: ago(45_000) });
    show([a], [lead]);
    await vi.waitFor(() => expect(within(detail()).getByRole('heading', { level: 2 })).toHaveTextContent('lead asks for approval'));
    const panel = detail();
    expect(panel.tagName).toBe('SECTION');
    expect(panel).toHaveClass('panel');
    expect(within(panel).getByText('Command approval')).toBeInTheDocument();
    expect(within(panel).getByRole('link', { name: `Run ${lead.id.slice(-8)}` })).toHaveAttribute('href', `#/runs/${lead.id}`);
    expect(within(panel).queryByRole('link', { name: /^Root / })).not.toBeInTheDocument();
    expect(panel).toHaveTextContent('waiting 45s');
    expect(within(panel).getByText('Action')).toBeInTheDocument();
    const code = within(panel).getByText('npm install --save-dev jest-retry');
    expect(code).toHaveClass('action');
    const terms = Array.from(panel.querySelectorAll('dt')).map(text);
    const values = Array.from(panel.querySelectorAll('dd')).map(text);
    expect(terms).toEqual(['Agent', 'Run budget']);
    expect(values).toEqual(['lead', '$3.05 spent of $4.00']);
    const buttons = within(panel).getAllByRole('button');
    expect(buttons.map(text)).toEqual(['Approve', 'Edit and approve', 'Reject']);
    expect(buttons[0]).toHaveClass('primary');
    expect(buttons[2]).toHaveClass('danger');
  });

  it('a question shows the answer box, Send answer and Reject', async () => {
    const r = run({ agent: 'planner' });
    show([approval(r, { action: 'Which branch should I release from?', question: true })], [r]);
    await vi.waitFor(() => expect(within(detail()).getByRole('heading', { level: 2 })).toHaveTextContent('planner asks a question'));
    const panel = detail();
    expect(within(panel).getByText('Question')).toBeInTheDocument();
    expect(within(panel).getByLabelText('Answer')).toBeInTheDocument();
    const buttons = within(panel).getAllByRole('button');
    expect(buttons.map(text)).toEqual(['Send answer', 'Reject']);
    expect(buttons[0]).toHaveClass('primary');
    expect(buttons[1]).toHaveClass('danger');
  });

  it('a child shows its root, a run without a limit says so, and an unloaded run says not known yet', async () => {
    const lead = run({ agent: 'lead' });
    const kid = run({ agent: 'fixer', parent: lead.id, root: lead.id, depth: 1, limit: null, spent: 0 });
    const lost = run({ agent: 'ghost' });
    show(
      [approval(kid, { requested_at: ago(10_000) }), approval(lost, { requested_at: ago(20_000) })],
      [lead, kid],
    );
    await vi.waitFor(() => expect(detail()).toHaveTextContent('fixer'));
    const panel = detail();
    expect(within(panel).getByRole('link', { name: `Root ${lead.id.slice(-8)}` })).toHaveAttribute('href', `#/runs/${lead.id}`);
    expect(Array.from(panel.querySelectorAll('dt')).map(text)).toEqual(['Agent', 'Run budget', 'Root']);
    expect(Array.from(panel.querySelectorAll('dd')).map(text).slice(0, 2)).toEqual(['fixer', 'no limit']);
    await user().click(pick(1));
    expect(Array.from(detail().querySelectorAll('dd')).map(text)).toEqual(['not known yet', 'not known yet']);
  });

  it('deciding the selected one selects the item now in its place and moves focus to it', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'one', requested_at: ago(10_000) });
    const b = approval(r, { action: 'two', requested_at: ago(20_000) });
    const c = approval(r, { action: 'three', requested_at: ago(30_000) });
    const { sockets, calls } = show([a, b, c], [r]);
    await vi.waitFor(() => expect(items()).toHaveLength(3));
    const u = user();
    await u.click(pick(1));
    await u.click(within(detail()).getByRole('button', { name: 'Approve' }));
    expect(posts(calls)).toEqual([{ method: 'POST', path: `/api/approvals/${b.id}/approve`, search: '', body: null }]);
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: b.id, approved: true }));
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    // `c` moved up into the place `b` had.
    expect(pick(1)).toHaveAttribute('aria-pressed', 'true');
    expect(detail()).toHaveTextContent('three');
    expect(document.activeElement).toBe(pick(1));
    // The last one going selects the one before it.
    await u.click(within(detail()).getByRole('button', { name: 'Approve' }));
    s.event(6, event(r.id, { kind: 'approval_resolved', id: c.id, approved: true }));
    await vi.waitFor(() => expect(items()).toHaveLength(1));
    expect(pick(0)).toHaveAttribute('aria-pressed', 'true');
    expect(document.activeElement).toBe(pick(0));
    // None left: no panel, focus on the heading.
    await u.click(within(detail()).getByRole('button', { name: 'Approve' }));
    s.event(7, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    await vi.waitFor(() => expect(screen.getByText('Nothing waiting for you')).toBeInTheDocument());
    expect(screen.queryByRole('region', { name: /asks/ })).not.toBeInTheDocument();
    expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1, name: 'Approvals' }));
  });

  it('an item that goes away while another is selected leaves the selection and focus alone', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'one', requested_at: ago(10_000) });
    const b = approval(r, { action: 'two', requested_at: ago(20_000) });
    const { sockets } = show([a, b], [r]);
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    await user().click(pick(1));
    const approve = within(detail()).getByRole('button', { name: 'Approve' });
    approve.focus();
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    await vi.waitFor(() => expect(items()).toHaveLength(1));
    expect(pick(0)).toHaveAttribute('aria-pressed', 'true');
    expect(document.activeElement).toBe(approve);
  });

  it('when someone else decided the selected one, says so and selects the next', async () => {
    const r = run();
    const a = approval(r, { action: 'one', requested_at: ago(10_000) });
    const b = approval(r, { action: 'two', requested_at: ago(20_000) });
    show([a, b], [r], { resolve: () => new Response(JSON.stringify({ error: 'approval is already resolved' }), { status: 409 }) });
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    await user().click(within(detail()).getByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(screen.getByRole('status')).toHaveTextContent('Already resolved by someone else'));
    expect(items()).toHaveLength(1);
    expect(detail()).toHaveTextContent('two');
    expect(document.activeElement).toBe(pick(0));
  });
});

describe('Approvals: recently decided (scenario 16)', () => {
  it('lists up to 10 decided approvals, newest first, with their outcome and agent', async () => {
    const planner = run({ agent: 'planner' });
    const mailer = run({ agent: 'mailer' });
    const statuses: ApprovalStatus[] = ['approved', 'rejected', 'expired'];
    const decided = Array.from({ length: 12 }, (_, i) =>
      approval(i % 2 === 0 ? planner : mailer, {
        action: `decided ${i}`,
        status: statuses[i % 3] as ApprovalStatus,
        requested_at: ago((i + 1) * 60_000),
      }),
    );
    const { calls } = show([], [planner, mailer], { decided: [...decided].reverse() });
    await vi.waitFor(() => expect(within(decidedList()).getAllByRole('listitem')).toHaveLength(10));
    expect(calls.filter((c) => c.path === '/api/approvals').map((c) => c.search).sort()).toEqual([
      '?status=approved',
      '?status=expired',
      '?status=pending',
      '?status=rejected',
    ]);
    const rows = within(decidedList()).getAllByRole('listitem');
    expect(rows.map((r) => r.querySelector('.action')?.textContent)).toEqual(
      Array.from({ length: 10 }, (_, i) => `decided ${i}`),
    );
    await vi.waitFor(() => expect(rows[0]).toHaveTextContent('planner'));
    expect(rows[1]).toHaveTextContent('mailer');
    expect(rows[0]).toHaveTextContent('Approved');
    expect(rows[1]).toHaveTextContent('Rejected');
    expect(rows[2]).toHaveTextContent('Expired');
    expect(rows[0]?.querySelector('.outcome')).toHaveClass('approved');
    expect(rows[1]?.querySelector('.outcome')).toHaveClass('rejected');
    expect(rows[2]?.querySelector('.outcome')).toHaveClass('expired');
    // Each run is asked for once.
    expect(calls.filter((c) => c.path === `/api/runs/${planner.id}`)).toHaveLength(1);
    expect(calls.filter((c) => c.path === `/api/runs/${mailer.id}`)).toHaveLength(1);
  });

  it('lists an approval once when two of the lists have it', async () => {
    const r = run({ agent: 'planner' });
    const a = approval(r, { status: 'approved', action: 'merge it' });
    show([], [r], { decided: [a, { ...a, status: 'expired' }] });
    await vi.waitFor(() => expect(within(decidedList()).getAllByRole('listitem')).toHaveLength(1));
  });

  it('says when nothing was decided yet', async () => {
    show([], []);
    expect(await screen.findByText('Nothing decided yet.')).toBeInTheDocument();
  });

  it('if a decided list fails to load, says so and the rest of the page still works', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r);
    const { calls } = show([a], [r], {
      decided: [approval(r, { status: 'approved', action: 'old one' })],
      failDecided: 'rejected',
    });
    expect(await screen.findByText('Could not load decided approvals.')).toBeInTheDocument();
    expect(screen.queryByText('old one')).not.toBeInTheDocument();
    await user().click(within(detail()).getByRole('button', { name: 'Approve' }));
    expect(posts(calls)).toEqual([{ method: 'POST', path: `/api/approvals/${a.id}/approve`, search: '', body: null }]);
  });
});

describe('Approvals: accessibility (scenario 17)', () => {
  it('Tab reaches the list buttons and then the detail controls, and axe finds nothing serious', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { requested_at: ago(10_000) });
    const q = approval(r, { question: true, action: 'Which branch?', requested_at: ago(20_000) });
    const { container } = show([a, q], [r], { decided: [approval(r, { status: 'rejected', action: 'old one' })] });
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    await screen.findByText('old one');
    const u = user();
    const order: HTMLElement[] = [pick(0), pick(1), within(detail()).getByRole('button', { name: 'Approve' })];
    for (let i = 0; i < 40 && document.activeElement !== order[0]; i++) await u.tab();
    expect(document.activeElement).toBe(order[0]);
    await u.tab();
    expect(document.activeElement).toBe(order[1]);
    for (let i = 0; i < 10 && document.activeElement !== order[2]; i++) await u.tab();
    expect(document.activeElement).toBe(order[2]);
    vi.useRealTimers();
    expect(ids(await seriousViolations(container))).toEqual([]);
    await userEvent.setup().click(pick(1));
    expect(ids(await seriousViolations(container))).toEqual([]);
  });
});

describe('Approvals: a long action is capped in the lists (PR #85 review)', () => {
  const huge = `${'x'.repeat(30_000)}\n${'y'.repeat(20_000)}`;
  const capped = `${'x'.repeat(120)}...`;

  it('shows the first line, capped, in the pending item and its button name, and in Recently decided', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: huge });
    const d = approval(r, { action: huge, status: 'approved', requested_at: ago(120_000) });
    show([a], [r], { decided: [d] });
    await vi.waitFor(() => expect(items()).toHaveLength(1));
    await vi.waitFor(() => expect(items()[0]).toHaveTextContent('lead'));
    const button = pick(0);
    expect(button.querySelector('.action')?.textContent).toBe(capped);
    expect(text(button)).toMatch(new RegExp(`^lead ${capped.replace(/\./g, '\\.')} Command, waiting 1m \\d\\ds$`));
    const name = new RegExp(`^lead ${capped.replace(/\./g, '\\.')} Command, waiting`);
    expect(within(pendingList()).getByRole('button', { name })).toBe(button);
    await vi.waitFor(() => expect(within(decidedList()).getAllByRole('listitem')).toHaveLength(1));
    const row = within(decidedList()).getByRole('listitem');
    const action = row.querySelector('.action') as HTMLElement;
    expect(action.textContent).toBe(capped);
    expect((action.getAttribute('title') ?? '').length).toBeLessThanOrEqual(capped.length);
    expect(text(row).length).toBeLessThan(200);
    // The detail panel keeps its 500-character preview with Show all.
    expect(detail().querySelector('pre.action')?.textContent).toBe(`${huge.slice(0, 500)}...`);
    expect(within(detail()).getByRole('button', { name: 'Show all' })).toBeInTheDocument();
  });
});

describe('Approvals: a decision on an approval that is not selected (PR #85 review)', () => {
  it('says in its list item while it is on its way and after it failed, announces the failure, and Retry works', async () => {
    const lead = run({ agent: 'lead' });
    const kid = run({ agent: 'kid' });
    const a = approval(lead, { action: 'deploy', requested_at: ago(10_000) });
    const b = approval(kid, { action: 'rm -rf build', requested_at: ago(20_000) });
    let answer: (res: Response) => void = () => {};
    let answers = 0;
    const { calls } = show([a, b], [lead, kid], {
      resolve: () => {
        answers += 1;
        if (answers > 1) return new Response(null, { status: 202 });
        return new Promise<Response>((done) => (answer = done));
      },
    });
    await vi.waitFor(() => expect(within(detail()).getByRole('heading', { level: 2 })).toHaveTextContent('lead asks'));
    const u = user();
    await u.click(within(detail()).getByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(items()[0]).toHaveTextContent('Approving'));
    await u.click(pick(1));
    expect(detail()).toHaveTextContent('rm -rf build');
    expect(items()[0]).toHaveTextContent('Approving');
    answer(new Response('{"error":"database is locked"}', { status: 500 }));
    await vi.waitFor(() => expect(items()[0]).toHaveTextContent('Failed, open it to retry'));
    expect(items()[0]).not.toHaveTextContent('Approving');
    expect(screen.getByRole('status')).toHaveTextContent('lead: the decision failed. Open it to retry.');
    // B is still selected.
    expect(pick(1)).toHaveAttribute('aria-pressed', 'true');
    await u.click(pick(0));
    expect(within(detail()).getByRole('alert')).toHaveTextContent('database is locked');
    await u.click(within(detail()).getByRole('button', { name: 'Retry' }));
    expect(posts(calls).map((c) => c.path)).toEqual([`/api/approvals/${a.id}/approve`, `/api/approvals/${a.id}/approve`]);
    await vi.waitFor(() => expect(items()[0]).toHaveTextContent('Approving'));
    expect(within(detail()).queryByRole('alert')).not.toBeInTheDocument();
  });

  it('says Rejecting and Sending answer while those are on their way', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'deploy', requested_at: ago(10_000) });
    const q = approval(r, { action: 'Which branch?', question: true, requested_at: ago(20_000) });
    show([a, q], [r], { resolve: () => new Promise<Response>(() => {}) });
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    const u = user();
    await u.click(within(detail()).getByRole('button', { name: 'Reject' }));
    await u.click(within(detail()).getByRole('button', { name: 'Confirm reject' }));
    await vi.waitFor(() => expect(items()[0]).toHaveTextContent('Rejecting'));
    await u.click(pick(1));
    await u.type(within(detail()).getByLabelText('Answer'), 'main');
    await u.click(within(detail()).getByRole('button', { name: 'Send answer' }));
    await vi.waitFor(() => expect(items()[1]).toHaveTextContent('Sending answer'));
    expect(items()[0]).toHaveTextContent('Rejecting');
  });

  it('a failure of the selected approval shows in its panel and is not announced again', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'deploy' });
    show([a], [r], { resolve: () => new Response('{"error":"database is locked"}', { status: 500 }) });
    await vi.waitFor(() => expect(items()).toHaveLength(1));
    await user().click(within(detail()).getByRole('button', { name: 'Approve' }));
    await vi.waitFor(() => expect(within(detail()).getByRole('alert')).toHaveTextContent('database is locked'));
    expect(items()[0]).toHaveTextContent('Failed, open it to retry');
    expect(screen.getByRole('status').textContent).toBe('');
  });
});

describe('Approvals: Recently decided stays current (PR #85 review)', () => {
  it('lists an approval decided on the page after its resolved event, with one refetch for several', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'deploy', requested_at: ago(10_000) });
    const b = approval(r, { action: 'publish', requested_at: ago(20_000) });
    const decided: Approval[] = [];
    const { sockets, calls } = show([a, b], [r], { decided });
    await vi.waitFor(() => expect(items()).toHaveLength(2));
    expect(await screen.findByText('Nothing decided yet.')).toBeInTheDocument();
    const asked = () => calls.filter((c) => c.path === '/api/approvals' && c.search === '?status=approved').length;
    expect(asked()).toBe(1);
    await user().click(within(detail()).getByRole('button', { name: 'Approve' }));
    decided.push({ ...a, status: 'approved' }, { ...b, status: 'approved' });
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'approval_resolved', id: a.id, approved: true }));
    s.event(6, event(r.id, { kind: 'approval_resolved', id: b.id, approved: true }));
    await vi.waitFor(() => expect(within(decidedList()).getAllByRole('listitem')).toHaveLength(2));
    expect(within(decidedList()).getAllByRole('listitem').map((li) => li.querySelector('.action')?.textContent)).toEqual([
      'deploy',
      'publish',
    ]);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(asked()).toBe(2);
  });

  it('does not refetch when an ended row is dismissed, and leaves out an approval that is in the pending rows', async () => {
    const r = run({ agent: 'lead' });
    const a = approval(r, { action: 'deploy', requested_at: ago(10_000) });
    const old = approval(r, { action: 'old one', status: 'rejected', requested_at: ago(600_000) });
    const { sockets, calls } = show([a], [r], { decided: [{ ...a, status: 'expired' }, old] });
    await vi.waitFor(() => expect(items()).toHaveLength(1));
    await screen.findByText('old one');
    const s = feed(sockets);
    s.open();
    s.event(5, event(r.id, { kind: 'status_changed', from: { Suspended: { Approval: a.id } }, to: 'Completed' }));
    await vi.waitFor(() => expect(within(detail()).getByText('Run ended')).toBeInTheDocument());
    expect(within(decidedList()).getAllByRole('listitem').map((li) => li.querySelector('.action')?.textContent)).toEqual([
      'old one',
    ]);
    const asked = () => calls.filter((c) => c.path === '/api/approvals' && c.search === '?status=expired').length;
    await user().click(within(detail()).getByRole('button', { name: 'Dismiss' }));
    await vi.advanceTimersByTimeAsync(2_000);
    expect(asked()).toBe(1);
    // Dismissed, it is no longer a pending row, so the list shows it with its outcome.
    expect(within(decidedList()).getAllByRole('listitem')).toHaveLength(2);
  });
});
