import {
  type ErrorMessage,
  type Event,
  type Frame,
  FrameError,
  FrameType,
  type Hello,
  type Welcome,
  encodeFrame,
  parseFrame,
  VERSION,
} from "@clankermux/proto";

import pkg from "../package.json" with { type: "json" };

/** A message-oriented byte transport: one frame per message, as over WebSocket. */
export interface Transport {
  send(bytes: Uint8Array): void;
  close(): void;
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;
}

/** `desync`: a stream frame did not continue from the cursor; a fresh
 * OPEN recovers it, where any other fault would only repeat. */
export type ErrorKind = "protocol" | "desync" | "server" | "closed";

export class ClientError extends Error {
  constructor(
    readonly kind: ErrorKind,
    message: string,
    readonly server?: ErrorMessage,
  ) {
    super(message);
  }

  static protocol(message: string): ClientError {
    return new ClientError("protocol", message);
  }

  static desync(message: string): ClientError {
    return new ClientError("desync", message);
  }

  static fromServer(error: ErrorMessage): ClientError {
    return new ClientError("server", `${error.code}: ${error.message}`, error);
  }
}

/** Frames before `WELCOME` tells us the real limit: the protocol's
 * default `max_frame`, as the Rust client. */
const HELLO_MAX_FRAME = 1024 * 1024;

export function hello(name: string, engine?: string): Hello {
  const h: Hello = { version: VERSION, client: { name, version: pkg.version }, capabilities: [] };
  if (engine !== undefined) h.engine = engine;
  return h;
}

/**
 * One protocol connection: the handshake, requests answered by id, events,
 * and attachment channels routed to their receivers.
 */
export class Connection {
  private nextId = 1;
  private readonly pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: ClientError) => void }>();
  private readonly channels = new Map<number, (frame: Frame) => void>();
  private ended: ClientError | undefined;
  /** Session events, once subscribed. */
  onEvent: ((event: Event) => void) | undefined;
  /** The connection ended: the transport closed, or an error without id. */
  onEnd: ((error: ClientError) => void) | undefined;

  private constructor(
    private readonly transport: Transport,
    readonly welcome: Welcome,
  ) {
    transport.onMessage = (bytes) => this.route(bytes);
    transport.onClose = () => this.end(new ClientError("closed", "connection closed"));
  }

  /** Sends `HELLO` and resolves at `WELCOME`; rejects on an `ERROR` or close. */
  static connect(transport: Transport, hello: Hello): Promise<Connection> {
    return new Promise((resolve, reject) => {
      transport.onMessage = (bytes) => {
        try {
          const frame = parseFrame(bytes, HELLO_MAX_FRAME);
          const body = JSON.parse(new TextDecoder().decode(frame.payload));
          if (frame.header.type === FrameType.Welcome) {
            resolve(new Connection(transport, body as Welcome));
          } else if (frame.header.type === FrameType.Error) {
            reject(ClientError.fromServer(body as ErrorMessage));
          } else {
            reject(ClientError.protocol(`${frame.header.type} before WELCOME`));
          }
        } catch (err) {
          reject(err instanceof ClientError ? err : ClientError.protocol(String(err)));
        }
      };
      transport.onClose = () => reject(new ClientError("closed", "closed before WELCOME"));
      transport.send(encodeFrame(FrameType.Hello, 0, new TextEncoder().encode(JSON.stringify(hello))));
    });
  }

  send(type: FrameType, channel: number, payload: Uint8Array): void {
    if (this.ended) throw this.ended;
    this.transport.send(encodeFrame(type, channel, payload));
  }

  sendJson(type: FrameType, channel: number, body: unknown): void {
    this.send(type, channel, new TextEncoder().encode(JSON.stringify(body)));
  }

  /** A request; the reply's `result`, or a rejection with the server's error. */
  call<R>(method: string, params: unknown): Promise<R> {
    if (this.ended) return Promise.reject(this.ended);
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      try {
        this.sendJson(FrameType.Request, 0, { id, method, params });
      } catch (err) {
        this.pending.delete(id);
        reject(err);
      }
    });
  }

  /** Opens `channel`; its frames go to `receive` until `release` or the
   * connection's end. */
  open(channel: number, open: unknown, receive: (frame: Frame) => void): void {
    if (this.ended) throw this.ended;
    this.channels.set(channel, receive);
    this.sendJson(FrameType.Open, channel, open);
  }

  /** Stops routing `channel`; later frames on it are dropped. */
  release(channel: number): void {
    this.channels.delete(channel);
  }

  close(): void {
    this.transport.close();
  }

  private route(bytes: Uint8Array): void {
    let frame: Frame;
    try {
      frame = parseFrame(bytes, this.welcome.limits.max_frame);
    } catch (err) {
      this.end(ClientError.protocol(err instanceof FrameError ? err.message : String(err)));
      return;
    }
    if (frame.header.channel !== 0) {
      this.channels.get(frame.header.channel)?.(frame);
      return;
    }
    let body: unknown;
    try {
      body = JSON.parse(new TextDecoder().decode(frame.payload));
    } catch (err) {
      this.end(ClientError.protocol(`malformed ${frame.header.type}: ${err}`));
      return;
    }
    switch (frame.header.type) {
      case FrameType.Reply: {
        // A reply or error for a request no longer waited for is dropped.
        const reply = body as { id: number; result: unknown };
        this.pending.get(reply.id)?.resolve(reply.result);
        this.pending.delete(reply.id);
        break;
      }
      case FrameType.Error: {
        const error = body as ErrorMessage;
        if (error.id === undefined) {
          this.end(ClientError.fromServer(error));
        } else {
          this.pending.get(error.id)?.reject(ClientError.fromServer(error));
          this.pending.delete(error.id);
        }
        break;
      }
      case FrameType.Event:
        this.onEvent?.(body as Event);
        break;
      default:
        this.end(ClientError.protocol(`${frame.header.type} on channel 0`));
    }
  }

  private end(error: ClientError): void {
    if (this.ended) return;
    this.ended = error;
    for (const waiter of this.pending.values()) waiter.reject(error);
    this.pending.clear();
    this.channels.clear();
    this.transport.close();
    this.onEnd?.(error);
  }
}
