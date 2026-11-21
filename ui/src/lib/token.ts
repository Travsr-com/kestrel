// The API token lives in memory only: never in localStorage, sessionStorage or a cookie, so it is
// gone when the tab closes and no other page on this origin can read it later.

let token: string | null = null;

export function getToken(): string | null {
  return token;
}

export function setToken(value: string): void {
  token = value;
}

export function clearToken(): void {
  token = null;
}

/**
 * True when `sent` is the token held now. A 401 for an older token (a request sent before the
 * user entered a new one) says nothing about the current token, so the app ignores it.
 */
export function isCurrent(sent: string | null): boolean {
  return sent === token;
}
