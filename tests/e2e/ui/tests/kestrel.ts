// What the suite gets from `tests/ui.rs`, and small readers for the API and the page.
import { expect, type APIRequestContext, type Page } from '@playwright/test';

/** One required environment variable. */
function need(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set: run this suite through tests/ui.rs (cargo test --test ui)`);
  return value;
}

/** The server and the runs `tests/ui.rs` started. */
export function kestrel() {
  return {
    /** The API token, as `kestrel up` printed it in the UI address. */
    token: need('KESTREL_TOKEN'),
    /** The root of the tree that grows while the browser watches. */
    root: need('KESTREL_ROOT'),
    /** How many children that root spawns. */
    children: Number(need('KESTREL_CHILDREN')),
    /** The root of the tree whose child asks a person. */
    askRoot: need('KESTREL_ASK_ROOT'),
    question: need('KESTREL_QUESTION'),
    answer: need('KESTREL_ANSWER'),
  };
}

/** One run, as `GET /api/runs/{id}` returns it. */
export interface RunSummary {
  id: string;
  agent: string;
  status: unknown;
  parent: string | null;
  root: string;
  spent: number;
  limit: number | null;
}

/** `GET path` with the token; must answer 200 with JSON. */
export async function api<T>(request: APIRequestContext, path: string): Promise<T> {
  const res = await request.get(path, { headers: { authorization: `Bearer ${kestrel().token}` } });
  expect(res.status(), `GET ${path}: ${await res.text()}`).toBe(200);
  return (await res.json()) as T;
}

/**
 * Micro-dollars as the UI shows them: at least cents, and as many more places as the exact amount
 * needs. A copy of `formatUsd` in `ui/src/lib/format.ts`, so the test checks the page against
 * the API's numbers, not against the UI's own code.
 */
export function formatUsd(micro: number | null): string {
  if (micro === null) return 'no limit';
  const abs = Math.abs(micro);
  const whole = Math.floor(abs / 1_000_000);
  let fraction = String(abs % 1_000_000).padStart(6, '0');
  while (fraction.length > 2 && fraction.endsWith('0')) fraction = fraction.slice(0, -1);
  return `${micro < 0 ? '-' : ''}$${whole}.${fraction}`;
}

/** One node of the spawn tree on the page: its run id and its status and cost ("Running, $0.01"). */
export interface Node {
  id: string;
  text: string;
}

/**
 * The spawn tree as the page shows it right now, read in one go. Each node has a status element
 * (`node-status-<id>`, which also holds a failed run's reason after a colon) and a cost element
 * (`node-cost-<id>`).
 */
export async function readTree(page: Page): Promise<Node[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('nav[aria-label="Spawn tree"] [id^="node-status-"]')).map((el) => {
      const id = el.id.slice('node-status-'.length);
      const clean = (s: string | null | undefined) => (s ?? '').replace(/\s+/g, ' ').trim();
      const status = clean(el.textContent).split(':')[0] ?? '';
      return { id, text: `${status}, ${clean(document.getElementById(`node-cost-${id}`)?.textContent)}` };
    }),
  );
}

/** The status part of a node's text: `Completed` of `Completed, $0.006465`. */
export function statusOf(node: Node): string {
  return node.text.split(',')[0] ?? '';
}
