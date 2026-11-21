// Event sockets that survive drops. After a drop the client waits (500 ms, doubling up to 10 s),
// reconnects with `after=<last cursor>`, and skips anything at or below that cursor, so the caller
// sees every event once, in order.
//
// Browsers cannot set headers on a WebSocket, so the token travels as a subprotocol:
// `kestrel.v1` (which the server echoes) plus `kestrel.token.<token>`. A close with code 4401 means
// the token was refused: the stream stops in state `unauthorized` instead of retrying. These values
// are `WS_PROTOCOL`, `WS_TOKEN_PREFIX` and `WS_CLOSE_UNAUTHORIZED` in `kestrel-types`.

import type { Event, WsFrame } from './types';

export const WS_PROTOCOL = 'kestrel.v1';
export const WS_TOKEN_PREFIX = 'kestrel.token.';
export const WS_CLOSE_UNAUTHORIZED = 4401;

export type ConnectionState = 'connecting' | 'open' | 'reconnecting' | 'unauthorized' | 'closed';

/** The part of `WebSocket` this module uses, so tests can pass a fake. */
export interface SocketLike {
  onopen: (() => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code: number }) => void) | null;
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
  ws.onclose = (ev) => socket.onclose?.(ev);
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
    const wait = delay;
    delay = Math.min(delay * 2, MAX_DELAY_MS);
    // Scheduled before the state is reported, so a `close()` from `onState` cancels it.
    retry = setTimeout(() => {
      retry = null;
      open();
    }, wait);
    state('reconnecting');
  }

  function open() {
    const token = opts.getToken();
    const protocols = token === null ? [WS_PROTOCOL] : [WS_PROTOCOL, `${WS_TOKEN_PREFIX}${token}`];
    state('connecting');
    // `onState` may have called `close()`.
    if (stopped) return;
    let s: SocketLike;
    try {
      s = factory(`${opts.base}${opts.path}?after=${cursor}`, protocols);
    } catch {
      socket = null;
      // `new WebSocket` throws on a malformed subprotocol. With a token, the token is what is
      // malformed and retrying would send it again, so stop and let the app ask for a new one.
      if (token !== null) stop('unauthorized');
      else drop();
      return;
    }
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
    s.onclose = (ev) => {
      if (ev.code === WS_CLOSE_UNAUTHORIZED) stop('unauthorized');
      else drop();
    };
  }

  /** Ends the stream for good. */
  function stop(final: 'closed' | 'unauthorized') {
    if (stopped) return;
    stopped = true;
    if (retry !== null) clearTimeout(retry);
    clearIdle();
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = null;
      socket.close();
    }
    state(final);
  }

  open();

  return {
    close: () => stop('closed'),
  };
}
