// Every view's rendered output has zero serious or critical axe violations, and each view can be
// operated with the keyboard alone.

import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import App from '../src/App.svelte';
import contract from './fixtures/contract.json';
import { seriousViolations, ids } from './axe';
import { createMockServer } from './mock-server';

const ROOT = contract.runs[0]!.id;

async function settle(hash: string, heading: string, token: string | null = null) {
  window.location.hash = hash;
  const { container } = render(App, { fetch: createMockServer(token).fetch });
  await screen.findByRole('heading', { level: 1, name: heading });
  return container;
}

describe('axe', () => {
  it.each([
    ['#/fleet', 'Fleet'],
    [`#/runs/${ROOT}`, `Run ${ROOT}`],
    ['#/approvals', 'Approvals'],
    ['#/nope', 'Not found'],
  ])('%s has no serious or critical violations', async (hash, heading) => {
    const container = await settle(hash, heading);
    await screen.findByTestId('ready');
    expect(ids(await seriousViolations(container))).toEqual([]);
  });

  it('the token screen has no serious or critical violations', async () => {
    const container = await settle('#/fleet', 'Token required', 'needed');
    expect(ids(await seriousViolations(container))).toEqual([]);
  });

  it('flags an image without alt text and a button without a name', async () => {
    const bad = document.createElement('main');
    bad.innerHTML = '<img src="x.png"><button></button>';
    document.body.append(bad);
    try {
      expect(ids(await seriousViolations(bad)).sort()).toEqual(['button-name', 'image-alt']);
    } finally {
      bad.remove();
    }
  });
});

describe('keyboard', () => {
  it('moves through the main navigation with Tab and follows a link with Enter', async () => {
    await settle('#/fleet', 'Fleet');
    const user = userEvent.setup();
    await user.tab();
    expect(document.activeElement).toHaveTextContent('Skip to content');
    await user.tab();
    expect(document.activeElement).toHaveTextContent('Fleet');
    await user.tab();
    expect(document.activeElement).toHaveTextContent('Approvals');
    await user.keyboard('{Enter}');
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    expect(await screen.findByRole('heading', { level: 1, name: 'Approvals' })).toBeInTheDocument();
  });

  it('reaches a run from the Fleet list with the keyboard', async () => {
    await settle('#/fleet', 'Fleet');
    await screen.findByTestId('ready');
    const user = userEvent.setup();
    const link = screen.getByRole('link', { name: /planner/ });
    for (let i = 0; i < 15 && document.activeElement !== link; i++) await user.tab();
    expect(document.activeElement).toBe(link);
    expect(link).toHaveAttribute('href', `#/runs/${ROOT}`);
  });

  it('submits the token form with Enter from the input', async () => {
    window.location.hash = '#/fleet';
    render(App, { fetch: createMockServer('k').fetch });
    const input = await screen.findByLabelText('API token');
    const user = userEvent.setup();
    await user.click(input);
    await user.keyboard('k{Enter}');
    expect(await screen.findByRole('heading', { level: 1, name: 'Fleet' })).toBeInTheDocument();
  });

  it('focuses the token input when the token screen appears', async () => {
    window.location.hash = '#/fleet';
    render(App, { fetch: createMockServer('k').fetch });
    const input = await screen.findByLabelText('API token');
    expect(document.activeElement).toBe(input);
  });
});
