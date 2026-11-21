import { Unauthorized } from './api';

/**
 * Turns a failed request into what a view shows. A 401 goes to `onUnauthorized` (the app then asks
 * for a token) and gives `null`; anything else gives the error text.
 */
export function failureText(error: unknown, onUnauthorized: () => void): string | null {
  if (error instanceof Unauthorized) {
    onUnauthorized();
    return null;
  }
  return error instanceof Error ? error.message : String(error);
}
