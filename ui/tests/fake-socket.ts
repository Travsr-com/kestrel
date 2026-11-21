// A WebSocket stand-in the tests drive by hand, and a factory that records every socket made.

import type { Event, WsFrame } from '../src/lib/types';
import type { SocketFactory, SocketLike } from '../src/lib/ws';

export class FakeSocket implements SocketLike {
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number }) => void) | null = null;
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
  frame(frame: WsFrame): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
  event(cursor: number, event: Event): void {
    this.frame({ type: 'event', cursor, event });
  }
  drop(code = 1006): void {
    this.onclose?.({ code });
  }
}

/** A factory whose sockets stay in `made`, in creation order. */
export function fakeSockets(): { factory: SocketFactory; made: FakeSocket[] } {
  const made: FakeSocket[] = [];
  return {
    made,
    factory: (url, protocols) => {
      const s = new FakeSocket(url, protocols);
      made.push(s);
      return s;
    },
  };
}
