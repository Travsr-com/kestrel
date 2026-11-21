// Scenario 7: when the socket drops, the client reconnects with backoff and resumes from the last
// cursor it saw, with no gap and no duplicate. Fake socket, fake timers.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import contract from './fixtures/contract.json';
import type { Event, WsFrame } from '../src/lib/types';
import { connectEvents, socketBase, type ConnectionState, type SocketLike } from '../src/lib/ws';

class FakeSocket implements SocketLike {
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  closed = false;
  constructor(
    readonly url: string,
    readonly protocols: string[],
  ) {}
  close(): void {
    this.closed = true;
  }
  open(): void {
    this.onopen?.();
  }
  frame(frame: WsFrame | string): void {
    this.onmessage?.({ data: typeof frame === 'string' ? frame : JSON.stringify(frame) });
  }
  drop(): void {
    this.onclose?.();
  }
}

const template = contract.events[2] as Event;

function event(cursor: number): WsFrame {
  return { type: 'event', cursor, event: { ...template, seq: cursor } };
}

let sockets: FakeSocket[];
let seen: number[];
let states: ConnectionState[];
let token: string | null;

function connect(after = 4) {
  return connectEvents({
    base: 'ws://kestrel.test',
    path: '/ws/runs/abc',
    after,
    getToken: () => token,
    onEvent: (_event, cursor) => seen.push(cursor),
    onState: (state) => states.push(state),
    socketFactory: (url, protocols) => {
      const s = new FakeSocket(url, protocols);
      sockets.push(s);
      return s;
    },
  });
}

function last(): FakeSocket {
  const s = sockets.at(-1);
  if (!s) throw new Error('no socket opened');
  return s;
}

beforeEach(() => {
  vi.useFakeTimers();
  sockets = [];
  seen = [];
  states = [];
  token = 't0k';
});

afterEach(() => {
  vi.useRealTimers();
});

describe('connectEvents', () => {
  it('opens at the given cursor with the token as a subprotocol', () => {
    connect(4);
    expect(last().url).toBe('ws://kestrel.test/ws/runs/abc?after=4');
    expect(last().protocols).toEqual(['kestrel.v1', 'kestrel.token.t0k']);
    expect(states).toEqual(['connecting']);
    last().open();
    expect(states).toEqual(['connecting', 'open']);
  });

  it('sends only the version subprotocol without a token', () => {
    token = null;
    connect();
    expect(last().protocols).toEqual(['kestrel.v1']);
  });

  it('reconnects after a drop and resumes from the last cursor with no gap and no duplicate', () => {
    connect(4);
    last().open();
    for (const c of [5, 6, 7]) last().frame(event(c));
    last().drop();
    expect(states.at(-1)).toBe('reconnecting');
    expect(sockets).toHaveLength(1);

    vi.advanceTimersByTime(499);
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(2);
    expect(last().url).toBe('ws://kestrel.test/ws/runs/abc?after=7');

    last().open();
    // The server may replay what the client already has; it must not reach onEvent twice.
    for (const c of [6, 7, 8, 9, 10]) last().frame(event(c));
    expect(seen).toEqual([5, 6, 7, 8, 9, 10]);
  });

  it('backs off exponentially up to 10 s and resets after a successful open', () => {
    connect();
    const delays: number[] = [];
    for (let i = 0; i < 7; i++) {
      const before = sockets.length;
      last().drop();
      let waited = 0;
      while (sockets.length === before) {
        vi.advanceTimersByTime(100);
        waited += 100;
      }
      delays.push(waited);
    }
    expect(delays).toEqual([500, 1000, 2000, 4000, 8000, 10000, 10000]);

    last().open();
    last().drop();
    vi.advanceTimersByTime(500);
    expect(sockets).toHaveLength(9);
  });

  it('asks for the token again on each connect', () => {
    connect();
    last().drop();
    token = 'n3w';
    vi.advanceTimersByTime(500);
    expect(last().protocols).toEqual(['kestrel.v1', 'kestrel.token.n3w']);
  });

  it('stops for good on close()', () => {
    const conn = connect();
    last().open();
    const socket = last();
    conn.close();
    expect(socket.closed).toBe(true);
    expect(states.at(-1)).toBe('closed');
    socket.drop();
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1);
    conn.close();
    expect(states.filter((s) => s === 'closed')).toHaveLength(1);
  });

  it('cancels a pending reconnect on close()', () => {
    const conn = connect();
    last().drop();
    conn.close();
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1);
  });

  it('ignores heartbeats and malformed frames', () => {
    connect(0);
    last().open();
    last().frame({ type: 'heartbeat' });
    last().frame('not json');
    last().frame('{"type":"mystery"}');
    last().frame('{"type":"event","cursor":"1"}');
    last().frame(event(1));
    last().onmessage?.({ data: new ArrayBuffer(2) });
    expect(seen).toEqual([1]);
  });

  it('treats 45 s without any frame as a dead socket and reconnects', () => {
    connect(0);
    last().open();
    vi.advanceTimersByTime(30_000);
    last().frame({ type: 'heartbeat' });
    vi.advanceTimersByTime(44_999);
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sockets[0]?.closed).toBe(true);
    expect(states.at(-1)).toBe('reconnecting');
    vi.advanceTimersByTime(500);
    expect(sockets).toHaveLength(2);
    // The dead socket's late close event must not schedule a second reconnect.
    last().open();
    sockets[0]?.drop();
    vi.advanceTimersByTime(10_000);
    expect(sockets).toHaveLength(2);
  });
});

describe('browserSocket', () => {
  class StubWebSocket {
    static last: StubWebSocket | null = null;
    onopen: (() => void) | null = null;
    onmessage: ((ev: { data: unknown }) => void) | null = null;
    onclose: (() => void) | null = null;
    closed = false;
    constructor(
      readonly url: string,
      readonly protocols: string[],
    ) {
      StubWebSocket.last = this;
    }
    close(): void {
      this.closed = true;
    }
  }

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('is the default and forwards open, message and close', () => {
    vi.stubGlobal('WebSocket', StubWebSocket);
    connectEvents({
      base: 'ws://kestrel.test',
      path: '/ws/tree/r',
      after: 0,
      getToken: () => null,
      onEvent: (_e, cursor) => seen.push(cursor),
      onState: (s) => states.push(s),
    });
    const ws = StubWebSocket.last;
    if (!ws) throw new Error('no WebSocket created');
    expect(ws.url).toBe('ws://kestrel.test/ws/tree/r?after=0');
    expect(ws.protocols).toEqual(['kestrel.v1']);
    ws.onopen?.();
    ws.onmessage?.({ data: JSON.stringify(event(3)) });
    expect(states).toEqual(['connecting', 'open']);
    expect(seen).toEqual([3]);
    ws.onclose?.();
    expect(states.at(-1)).toBe('reconnecting');
  });

  it('closes the real socket', () => {
    vi.stubGlobal('WebSocket', StubWebSocket);
    const conn = connectEvents({
      base: 'ws://kestrel.test',
      path: '/ws/runs/a',
      after: 0,
      getToken: () => null,
      onEvent: () => {},
    });
    conn.close();
    expect(StubWebSocket.last?.closed).toBe(true);
  });
});

describe('socketBase', () => {
  it('maps http to ws and https to wss', () => {
    expect(socketBase({ protocol: 'http:', host: 'localhost:7171' })).toBe('ws://localhost:7171');
    expect(socketBase({ protocol: 'https:', host: 'kestrel.example' })).toBe('wss://kestrel.example');
  });
});
