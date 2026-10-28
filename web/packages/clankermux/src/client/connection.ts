import {
  type Catalog,
  type Description,
  type ErrorCode,
  type ErrorMessage,
  type Event,
  type Frame,
  FrameError,
  FrameType,
  type Hello,
  type Peer,
  type Reply,
  type Welcome,
  encodeFrame,
  parseFrame,
  VERSION,
} from "../proto/index.ts";

import pkg from "../../package.json" with { type: "json" };

export interface Transport {
  send(bytes: Uint8Array): void;
  close(): void;
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;
}

export type ErrorKind = "protocol" | "desync" | "server" | "closed";

/** A failed request or connection. `kind: "server"` carries the server's
 * `ERROR` as `server`, and its code as `code`: a refused request, or a
 * `wait` that was cancelled (`cancelled`). */
export class ClientError extends Error {
  /** The protocol's error code, when the server sent one. */
  readonly code: ErrorCode | undefined;

  constructor(
    readonly kind: ErrorKind,
    message: string,
    readonly server?: ErrorMessage,
  ) {
    super(message);
    this.name = "ClientError";
    this.code = server?.code;
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

const HELLO_MAX_FRAME = 1024 * 1024;

export function hello(client: string | Peer, engine?: string): Hello {
  const peer = typeof client === "string" ? { name: client, version: pkg.version } : client;
  const h: Hello = { version: VERSION, client: peer, capabilities: [] };
  if (engine !== undefined) h.engine = engine;
  return h;
}

export class Connection {
  private nextId = 1;
  private readonly pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: ClientError) => void }>();
  private readonly channels = new Map<number, (frame: Frame) => void>();
  private ended: ClientError | undefined;
  onEvent: ((event: Event) => void) | undefined;
  onEnd: ((error: ClientError) => void) | undefined;

  private constructor(
    private readonly transport: Transport,
    readonly welcome: Welcome,
  ) {
    transport.onMessage = (bytes) => this.route(bytes);
    transport.onClose = () => this.end(new ClientError("closed", "connection closed"));
  }

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
          reject(ClientError.protocol(String(err)));
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

  call<R>(method: string, params: unknown): Promise<R> {
    return this.request<R>(method, params).reply;
  }

  request<R>(method: string, params: unknown): { id: number; reply: Promise<R> } {
    const id = this.nextId++;
    const reply = new Promise<R>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      try {
        this.sendJson(FrameType.Request, 0, { id, method, params });
      } catch (err) {
        this.pending.delete(id);
        reject(err);
      }
    });
    return { id, reply };
  }

  describe(): Promise<Catalog>;
  describe(method: string): Promise<Description>;
  describe(method?: string): Promise<Catalog | Description> {
    return this.call("describe", method === undefined ? {} : { method });
  }

  open(channel: number, open: unknown, receive: (frame: Frame) => void): void {
    if (this.ended) throw this.ended;
    this.channels.set(channel, receive);
    this.sendJson(FrameType.Open, channel, open);
  }

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
        const reply = body as Reply;
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
