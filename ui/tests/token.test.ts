// Scenario 6: a 401 shows "Token required"; the token entered is kept in memory only, and the
// request is retried with it.

import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import { clearToken, getToken, setToken } from '../src/lib/token';
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
    expect(await screen.findByText('planner')).toBeInTheDocument();
    expect(server.calls.at(-1)?.auth).toBe('Bearer s3cret');
    expect(getToken()).toBe('s3cret');
    expect(setItem).not.toHaveBeenCalled();
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
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
