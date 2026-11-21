import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/svelte';
import { afterEach } from 'vitest';
import { clearToken } from '../src/lib/token';

afterEach(() => {
  cleanup();
  clearToken();
  window.location.hash = '';
});
