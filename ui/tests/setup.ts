import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/svelte';
import { afterEach, vi } from 'vitest';
import { clearToken } from '../src/lib/token';

// Views open event sockets. jsdom's WebSocket would try a real connection and then retry; this
// one never opens or closes, so tests that do not drive sockets themselves see a quiet, open
// connection attempt. Tests that need sockets pass a fake factory.
class InertWebSocket {
  onopen: (() => void) | null = null;
  onmessage: (() => void) | null = null;
  onclose: (() => void) | null = null;
  close(): void {}
}
vi.stubGlobal('WebSocket', InertWebSocket);

afterEach(() => {
  cleanup();
  clearToken();
  window.location.hash = '';
});
