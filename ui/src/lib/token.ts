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
