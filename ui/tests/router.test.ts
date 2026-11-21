// Scenario 5: hash routes pick the view; an unknown hash shows "Not found" with a link to Fleet.

import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import App from '../src/App.svelte';
import { href, parseRoute } from '../src/lib/router';
import { createMockServer } from './mock-server';

describe('parseRoute', () => {
  it.each([
    ['', { name: 'fleet' }],
    ['#', { name: 'fleet' }],
    ['#/', { name: 'fleet' }],
    ['#/fleet', { name: 'fleet' }],
    ['#/approvals', { name: 'approvals' }],
    ['#/runs/abc', { name: 'run', id: 'abc' }],
    ['#/runs/a%20b', { name: 'run', id: 'a b' }],
    ['#/runs/', { name: 'notfound' }],
    ['#/runs/abc/extra', { name: 'notfound' }],
    ['#/nope', { name: 'notfound' }],
    ['#/runs/%E0%A4%A', { name: 'notfound' }],
  ])('parses %s', (hash, route) => {
    expect(parseRoute(hash)).toEqual(route);
  });
});

describe('href', () => {
  it('builds the hash for each route', () => {
    expect(href({ name: 'fleet' })).toBe('#/fleet');
    expect(href({ name: 'approvals' })).toBe('#/approvals');
    expect(href({ name: 'run', id: 'a b' })).toBe('#/runs/a%20b');
  });
  it('round-trips through parseRoute', () => {
    const route = { name: 'run', id: 'x/y?z' } as const;
    expect(parseRoute(href(route))).toEqual(route);
  });
});

describe('App routing', () => {
  it('mounts the Run view with the id from #/runs/abc', async () => {
    window.location.hash = '#/runs/abc';
    render(App, { fetch: createMockServer().fetch });
    expect(await screen.findByRole('heading', { level: 1, name: 'Run abc' })).toBeInTheDocument();
  });

  it('renders Not found with a link to Fleet for an unknown hash', async () => {
    window.location.hash = '#/nope';
    render(App, { fetch: createMockServer().fetch });
    expect(await screen.findByRole('heading', { level: 1, name: 'Not found' })).toBeInTheDocument();
    const main = screen.getByRole('main');
    const link = Array.from(main.querySelectorAll('a')).find((a) => a.textContent === 'Go to Fleet');
    expect(link).toHaveAttribute('href', '#/fleet');
  });

  it('follows hash changes', async () => {
    window.location.hash = '#/fleet';
    render(App, { fetch: createMockServer().fetch });
    expect(await screen.findByRole('heading', { level: 1, name: 'Fleet' })).toBeInTheDocument();
    window.location.hash = '#/approvals';
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    expect(await screen.findByRole('heading', { level: 1, name: 'Approvals' })).toBeInTheDocument();
  });

  it('marks the current page in the main navigation', async () => {
    window.location.hash = '#/approvals';
    render(App, { fetch: createMockServer().fetch });
    const nav = screen.getByRole('navigation', { name: 'Main' });
    const current = nav.querySelector('[aria-current="page"]');
    expect(current).toHaveTextContent('Approvals');
  });
});
