// The scaffold views: empty lists, server errors, and the links between views.

import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import App from '../src/App.svelte';
import { failureText } from '../src/lib/load';
import { Unauthorized } from '../src/lib/api';
import contract from './fixtures/contract.json';
import { createMockServer } from './mock-server';

const ROOT = contract.runs[0]!.id;
const CHILD = contract.runs[1]!.id;

/** A server that answers every request with `status` and `body`. */
function fixed(status: number, body: unknown): typeof fetch {
  return async () => new Response(JSON.stringify(body), { status });
}

function show(hash: string, serve: typeof fetch) {
  window.location.hash = hash;
  return render(App, { fetch: serve });
}

describe('empty lists', () => {
  it('Fleet says there are no runs', async () => {
    show('#/fleet', fixed(200, []));
    expect(await screen.findByText('No runs yet.')).toBeInTheDocument();
  });

  it('Approvals says there is nothing pending', async () => {
    show('#/approvals', fixed(200, []));
    expect(await screen.findByText('Nothing waiting for you')).toBeInTheDocument();
  });
});

describe('server errors', () => {
  it.each([
    ['#/fleet', 'Fleet'],
    ['#/approvals', 'Approvals'],
    [`#/runs/${ROOT}`, `Run ${ROOT}`],
  ])('%s shows the error text', async (hash, heading) => {
    show(hash, fixed(500, { error: 'database is locked' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('database is locked');
    expect(screen.getByRole('heading', { level: 1, name: heading })).toBeInTheDocument();
  });
});

describe('Run view', () => {
  it('shows the run and links to its parent', async () => {
    show(`#/runs/${CHILD}`, createMockServer().fetch);
    expect(await screen.findByRole('heading', { level: 1, name: 'fixer' })).toBeInTheDocument();
    expect(screen.getAllByText('Suspended: waiting for approval').length).toBeGreaterThan(0);
    expect(screen.getByText('$0.125 of $0.50, $0.00 carved to children')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Parent: planner' })).toHaveAttribute('href', `#/runs/${ROOT}`);
  });

  it('has no parent link for a root', async () => {
    show(`#/runs/${ROOT}`, createMockServer().fetch);
    await screen.findByRole('heading', { level: 1, name: 'planner' });
    expect(screen.queryByText(/^Parent/)).not.toBeInTheDocument();
  });

  it('says when no run has the id, with a way back', async () => {
    show('#/runs/missing', createMockServer().fetch);
    expect(await screen.findByText('Run not found.')).toBeInTheDocument();
    const main = screen.getByRole('main');
    expect(Array.from(main.querySelectorAll('a')).map((a) => a.getAttribute('href'))).toEqual(['#/fleet']);
  });

  it('loads the new run when the id in the hash changes', async () => {
    show(`#/runs/${ROOT}`, createMockServer().fetch);
    await screen.findByRole('heading', { level: 1, name: 'planner' });
    window.location.hash = `#/runs/${CHILD}`;
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    expect(await screen.findByRole('heading', { level: 1, name: 'fixer' })).toBeInTheDocument();
  });
});

describe('Approvals view', () => {
  it('lists pending approvals with a link to the run', async () => {
    show('#/approvals', createMockServer().fetch);
    expect(await screen.findByText('git push origin kestrel/run-9a8b')).toBeInTheDocument();
    expect(screen.queryByText('delete branch old-work')).not.toBeInTheDocument();
    expect(screen.getByRole('link', { name: `Run ${CHILD.slice(-8)}` })).toHaveAttribute('href', `#/runs/${CHILD}`);
  });
});

describe('skip link', () => {
  it('moves focus to the main content without changing the route', async () => {
    show('#/approvals', createMockServer().fetch);
    await screen.findByTestId('ready');
    const user = userEvent.setup();
    await user.click(screen.getByText('Skip to content'));
    expect(document.activeElement).toBe(screen.getByRole('main'));
    expect(window.location.hash).toBe('#/approvals');
  });
});

describe('failureText', () => {
  it('sends a 401 to the handler with the token it carried, and shows nothing', () => {
    const sent: (string | null)[] = [];
    expect(failureText(new Unauthorized('t0k'), (token) => sent.push(token))).toBeNull();
    expect(sent).toEqual(['t0k']);
  });

  it('shows the message of an Error, or the value itself', () => {
    const never = () => {
      throw new Error('not a 401');
    };
    expect(failureText(new Error('boom'), never)).toBe('boom');
    expect(failureText('offline', never)).toBe('offline');
  });
});
