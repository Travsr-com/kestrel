// Event sockets that survive drops. After a drop the client waits (500 ms, doubling up to 10 s),
// reconnects with `after=<last cursor>`, and skips anything at or below that cursor, so the caller
// sees every event once, in order.
//
// Browsers cannot set headers on a WebSocket, so the token travels as a subprotocol:
// `kestrel.v1` (which the server echoes) plus `kestrel.token.<token>`.

import type { Event, WsFrame } from './types';

export type ConnectionState = 'connecting' | 'open' | 'reconnecting' | 'closed';

/** The part of `WebSocket` this module uses, so tests can pass a fake. */
export interface SocketLike {
  onopen: (() => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  close(): void;
}

export type SocketFactory = (url: string, protocols: string[]) => SocketLike;

export interface EventStreamOptions {
  /** `ws://host` or `wss://host`; see `socketBase`. */
  base: string;
  /** `/ws/runs/<id>` or `/ws/tree/<root>`. */
  path: string;
  /** Start after this cursor (0 for everything). */
  after: number;
  getToken: () => string | null;
  onEvent: (event: Event, cursor: number) => void;
  onState?: (state: ConnectionState) => void;
  socketFactory?: SocketFactory;
}

export interface EventStream {
  close(): void;
}

const INITIAL_DELAY_MS = 500;
const MAX_DELAY_MS = 10_000;
/** Three missed 15 s heartbeats: the connection is dead even if no close event came. */
const IDLE_MS = 45_000;

/** A real `WebSocket` behind the `SocketLike` interface. */
export const browserSocket: SocketFactory = (url, protocols) => {
  const ws = new WebSocket(url, protocols);
  const socket: SocketLike = { onopen: null, onmessage: null, onclose: null, close: () => ws.close() };
  ws.onopen = () => socket.onopen?.();
  ws.onmessage = (ev) => socket.onmessage?.(ev);
  ws.onclose = () => socket.onclose?.();
  return socket;
};

export function socketBase(loc: { protocol: string; host: string }): string {
  return `${loc.protocol === 'https:' ? 'wss' : 'ws'}://${loc.host}`;
}

function parseFrame(data: unknown): WsFrame | null {
  if (typeof data !== 'string') return null;
  let frame: unknown;
  try {
    frame = JSON.parse(data);
  } catch {
    return null;
  }
  if (typeof frame !== 'object' || frame === null) return null;
  const f = frame as Record<string, unknown>;
  if (f.type === 'heartbeat') return { type: 'heartbeat' };
  if (f.type === 'event' && typeof f.cursor === 'number' && typeof f.event === 'object' && f.event !== null) {
    return { type: 'event', cursor: f.cursor, event: f.event as Event };
  }
  return null;
}

export function connectEvents(opts: EventStreamOptions): EventStream {
  const factory = opts.socketFactory ?? browserSocket;
  let cursor = opts.after;
  let delay = INITIAL_DELAY_MS;
  let socket: SocketLike | null = null;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let idle: ReturnType<typeof setTimeout> | null = null;
  let stopped = false;

  const state = (s: ConnectionState) => opts.onState?.(s);

  function clearIdle() {
    if (idle !== null) clearTimeout(idle);
    idle = null;
  }

  function armIdle() {
    clearIdle();
    idle = setTimeout(() => {
      drop();
      socket?.close();
    }, IDLE_MS);
  }

  /** Forget the current socket and schedule the next attempt. */
  function drop() {
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = null;
    }
    clearIdle();
    state('reconnecting');
    const wait = delay;
    delay = Math.min(delay * 2, MAX_DELAY_MS);
    retry = setTimeout(() => {
      retry = null;
      open();
    }, wait);
  }

  function open() {
    const token = opts.getToken();
    const protocols = token === null ? ['kestrel.v1'] : ['kestrel.v1', `kestrel.token.${token}`];
    state('connecting');
    const s = factory(`${opts.base}${opts.path}?after=${cursor}`, protocols);
    socket = s;
    armIdle();
    s.onopen = () => {
      delay = INITIAL_DELAY_MS;
      state('open');
    };
    s.onmessage = (ev) => {
      armIdle();
      const frame = parseFrame(ev.data);
      if (frame?.type !== 'event' || frame.cursor <= cursor) return;
      cursor = frame.cursor;
      opts.onEvent(frame.event, frame.cursor);
    };
    s.onclose = () => drop();
  }

  open();

  return {
    close() {
      if (stopped) return;
      stopped = true;
      if (retry !== null) clearTimeout(retry);
      clearIdle();
      if (socket) {
        socket.onopen = socket.onmessage = socket.onclose = null;
        socket.close();
      }
      state('closed');
    },
  };
}
