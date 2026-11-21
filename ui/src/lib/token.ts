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
 * Takes the token from `?token=` in the address `kestrel up` prints, keeps it in memory, and
 * rewrites the URL without it (through `replace`, normally `history.replaceState`) so it does not
 * stay in the address bar or the history. Returns whether there was one.
 */
export function takeTokenFromUrl(
  loc: { search: string; pathname: string; hash: string },
  replace: (url: string) => void,
): boolean {
  const params = new URLSearchParams(loc.search);
  const found = params.get('token');
  if (!found) return false;
  setToken(found);
  params.delete('token');
  const rest = params.toString();
  replace(`${loc.pathname}${rest ? `?${rest}` : ''}${loc.hash}`);
  return true;
}

/**
 * True when `sent` is the token held now. A 401 for an older token (a request sent before the
 * user entered a new one) says nothing about the current token, so the app ignores it.
 */
export function isCurrent(sent: string | null): boolean {
  return sent === token;
}
