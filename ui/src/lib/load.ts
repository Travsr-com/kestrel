import { Unauthorized } from './api';

/** Called with the token a request carried when the server answered 401. */
export type OnUnauthorized = (sent: string | null) => void;

/**
 * Turns a failed request into what a view shows. A 401 goes to `onUnauthorized` (the app then asks
 * for a token) and gives `null`; anything else gives the error text.
 */
export function failureText(error: unknown, onUnauthorized: OnUnauthorized): string | null {
  if (error instanceof Unauthorized) {
    onUnauthorized(error.token);
    return null;
  }
  return error instanceof Error ? error.message : String(error);
}

/**
 * Runs `request` for a view and hands the result to `done` or the error to `failed`, unless the
 * view went away first. Returns the cleanup for `$effect`, so a late answer, including a 401, never
 * reaches a view that is gone.
 */
export function whileMounted<T>(
  request: () => Promise<T>,
  done: (value: T) => void,
  failed: (error: unknown) => void,
): () => void {
  let alive = true;
  request().then(
    (value) => {
      if (alive) done(value);
    },
    (error: unknown) => {
      if (alive) failed(error);
    },
  );
  return () => {
    alive = false;
  };
}
