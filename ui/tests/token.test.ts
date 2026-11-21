// Scenario 6: a 401 shows "Token required"; the token entered is kept in memory only, and the
// request is retried with it.

import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import { clearToken, getToken, isCurrent, setToken, takeTokenFromUrl } from '../src/lib/token';
import { createMockServer } from './mock-server';

afterEach(() => {
  vi.restoreAllMocks();
});

describe('token store', () => {
  it('holds the token in memory', () => {
    expect(getToken()).toBeNull();
    setToken('abc');
    expect(getToken()).toBe('abc');
    clearToken();
    expect(getToken()).toBeNull();
  });

  it('only treats a 401 for the current token as a reason to ask again', () => {
    setToken('new');
    expect(isCurrent('new')).toBe(true);
    expect(isCurrent('old')).toBe(false);
    expect(isCurrent(null)).toBe(false);
    clearToken();
    expect(isCurrent(null)).toBe(true);
  });
});

describe('late answers', () => {
  // Today the view that sent the old request is always gone by the time a token is entered, so
  // `whileMounted` drops the answer; `isCurrent` (tested above) is the second line of defence for
  // requests that outlive a token change, such as future command buttons.
  it('a 401 for a request sent before the token was entered does not undo the token', async () => {
    const server = createMockServer('s3cret');
    let release = () => {};
    const held = new Promise<void>((resolve) => (release = resolve));
    let first = true;
    const slowFirst: typeof fetch = async (input, init) => {
      if (first) {
        first = false;
        await held;
      }
      return server.fetch(input, init);
    };

    // Fleet's request (no token yet) hangs; the user moves on to Approvals and enters the token.
    window.location.hash = '#/fleet';
    render(App, { fetch: slowFirst });
    window.location.hash = '#/approvals';
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    const user = userEvent.setup();
    await user.type(await screen.findByLabelText('API token'), 's3cret{Enter}');
    expect(await screen.findByText('git push origin kestrel/run-9a8b')).toBeInTheDocument();

    // Fleet's old request now comes back 401.
    release();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(getToken()).toBe('s3cret');
    expect(screen.getByRole('heading', { level: 1, name: 'Approvals' })).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('a 401 that arrives after its view is gone does not ask for a token', async () => {
    let release = () => {};
    const held = new Promise<void>((resolve) => (release = resolve));
    const server = createMockServer('s3cret');
    const slow: typeof fetch = async (input, init) => {
      await held;
      return server.fetch(input, init);
    };
    // Same token (none) before and after, so only the unmount tells this answer apart.
    window.location.hash = '#/fleet';
    render(App, { fetch: slow });
    window.location.hash = '#/nope';
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    await screen.findByRole('heading', { level: 1, name: 'Not found' });
    release();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(screen.getByRole('heading', { level: 1, name: 'Not found' })).toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: 'Token required' })).not.toBeInTheDocument();
  });
});

describe('Token required screen', () => {
  it('appears on 401, then retries with the entered token', async () => {
    const setItem = vi.spyOn(Storage.prototype, 'setItem');
    const server = createMockServer('s3cret');
    window.location.hash = '#/fleet';
    render(App, { fetch: server.fetch });

    expect(await screen.findByRole('heading', { level: 1, name: 'Token required' })).toBeInTheDocument();
    const input = screen.getByLabelText('API token');
    expect(input).toHaveAttribute('type', 'password');

    const user = userEvent.setup();
    await user.type(input, 's3cret');
    await user.click(screen.getByRole('button', { name: 'Continue' }));

    expect(await screen.findByRole('heading', { level: 1, name: 'Fleet' })).toBeInTheDocument();
    expect(await screen.findByRole('link', { name: 'planner' })).toBeInTheDocument();
    expect(server.calls.at(-1)?.auth).toBe('Bearer s3cret');
    expect(getToken()).toBe('s3cret');
    expect(setItem).not.toHaveBeenCalled();
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
    expect(document.cookie).toBe('');
  });

  it('says the token was refused and asks again when it is wrong', async () => {
    const server = createMockServer('s3cret');
    render(App, { fetch: server.fetch });
    const user = userEvent.setup();
    await user.type(await screen.findByLabelText('API token'), 'wrong{Enter}');
    expect(await screen.findByRole('alert')).toHaveTextContent('That token was refused.');
    expect(screen.getByRole('heading', { level: 1, name: 'Token required' })).toBeInTheDocument();
    expect(getToken()).toBeNull();
  });

  it('does not submit an empty token', async () => {
    const server = createMockServer('s3cret');
    render(App, { fetch: server.fetch });
    const user = userEvent.setup();
    await user.click(await screen.findByRole('button', { name: 'Continue' }));
    expect(screen.getByRole('heading', { level: 1, name: 'Token required' })).toBeInTheDocument();
    expect(server.calls).toHaveLength(1);
  });
});

describe('token from the URL', () => {
  it('takes ?token= from the address kestrel up prints and removes it from the URL', () => {
    const replaced: string[] = [];
    const took = takeTokenFromUrl(
      { search: '?token=abc123', pathname: '/', hash: '#/fleet' },
      (url) => replaced.push(url),
    );
    expect(took).toBe(true);
    expect(getToken()).toBe('abc123');
    expect(replaced).toEqual(['/#/fleet']);
  });

  it('keeps other query parameters', () => {
    const replaced: string[] = [];
    takeTokenFromUrl({ search: '?a=1&token=t&b=2', pathname: '/ui', hash: '' }, (url) => replaced.push(url));
    expect(getToken()).toBe('t');
    expect(replaced).toEqual(['/ui?a=1&b=2']);
  });

  it('does nothing without a token', () => {
    const replaced: string[] = [];
    expect(takeTokenFromUrl({ search: '', pathname: '/', hash: '' }, (url) => replaced.push(url))).toBe(false);
    expect(takeTokenFromUrl({ search: '?token=', pathname: '/', hash: '' }, (url) => replaced.push(url))).toBe(false);
    expect(getToken()).toBeNull();
    expect(replaced).toEqual([]);
  });
});
