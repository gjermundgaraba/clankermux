/** The page side of a client: one worker, one connection, the control
 * surface, and the terminals mounted from it. */
import type {
  Acked,
  Catalog,
  CreateParams,
  Description,
  Event,
  Method,
  Methods,
  Page,
  Peer,
  Read,
  ReadParams,
  ResizeParams,
  SendParams,
  Sent,
  Session,
  StreamCursor,
  Tail,
  Until,
  Waited,
} from "../proto/index.ts";

import { ClientError } from "./connection.ts";
import { Listeners } from "./listeners.ts";
import { type MountHost, type MountOptions, type Terminal, mountOn } from "./mount.ts";
import type { Failure, Link, ToPage, ToWorker } from "./worker.ts";

export interface ConnectOptions {
  /** The server's WebSocket carrier, `wss://<node>:<port>/`. */
  url: string;
  /** A module worker whose script calls `runWorker(self)`; the client
   * owns it from here. */
  worker: Worker;
  /** ghostty-vt.wasm, resolved against the page. */
  wasm: string;
  /** The name and version the client gives in `HELLO`, which the
   * session's `attachments` list; `clankermux-web` and this package's
   * version by default. */
  client?: Peer;
}

/** Events were lost: the server restarted (a new `incarnation`), or more
 * happened while the connection was down than it retains. The session
 * list was paged again, and `onSessions` has it. */
export interface EventGap {
  event: "gap";
  incarnation: string;
}

/** `send`'s input: exactly one of `text`, `keys` and `data`. */
export type SendInput = Omit<SendParams, "session">;

export interface WaitOptions {
  /** The cursor to wait from; now by default. */
  after?: StreamCursor;
  timeout_ms?: number;
  /** Aborting it cancels the wait, which rejects with `cancelled`. */
  signal?: AbortSignal;
}

/**
 * Starts the worker and its connection to the server at `url`. Resolves
 * once the engine is loaded, before the connection is up: the worker
 * connects, and reconnects after a loss, by itself, and the client's
 * `onLink` (and each mounted terminal's) says how it goes. Rejects when
 * the worker or the engine cannot be loaded.
 */
export function connect(options: ConnectOptions): Promise<Client> {
  return new Promise((resolve, reject) => {
    const client = new Client(options.worker, (error) => {
      if (error === undefined) resolve(client);
      else reject(error);
    });
    client.post({
      type: "init",
      wasm: new URL(options.wasm, location.href).toString(),
      url: options.url,
      client: options.client,
    });
  });
}

/**
 * One connection to a server: each method is the protocol method of the
 * same name, and rejects with a `ClientError` (the server's `code`, or
 * `closed` when there is no connection). Terminals mount from it and share
 * its connection.
 */
export class Client {
  private nextId = 1;
  private readonly pending = new Map<
    number,
    { resolve: (result: unknown) => void; reject: (err: Error) => void; done: () => void }
  >();
  private readonly mounts = new Map<number, { terminal: Terminal; receive: (message: ToPage) => void }>();
  private readonly sessionListeners = new Listeners<Session[]>();
  private readonly linkListeners = new Listeners<Link>();
  private readonly eventListeners = new Set<(event: Event | EventGap) => void>();
  private started = false;
  private closed = false;

  /** @internal Use `connect`. */
  constructor(
    private readonly worker: Worker,
    started: (error?: Error) => void,
  ) {
    // A worker script that does not load posts nothing, so `connect`
    // would never settle. Once it runs, an error event is an exception in
    // one of its handlers, which costs that message only: the browser
    // reports it, and the client goes on.
    worker.onerror = (e) => {
      if (this.started) return;
      e.preventDefault();
      started(new Error(`the worker failed: ${e.message || "it did not load"}`));
      this.close();
    };
    worker.onmessage = (e: MessageEvent<ToPage>) => {
      const message = e.data;
      switch (message.type) {
        case "ready":
          this.started = true;
          started();
          break;
        case "failed":
          started(new Error(message.message));
          this.close();
          break;
        case "reply": {
          const waiter = this.pending.get(message.id);
          this.pending.delete(message.id);
          waiter?.done();
          if (message.error) waiter?.reject(errorOf(message.error));
          else waiter?.resolve(message.result);
          break;
        }
        case "connection":
          this.linkListeners.emit(message.link);
          break;
        case "sessions":
          this.sessionListeners.emit(message.sessions);
          break;
        case "event":
          for (const listener of this.eventListeners) listener(message.event);
          break;
        case "gap":
          for (const listener of this.eventListeners) listener({ event: "gap", incarnation: message.incarnation });
          break;
        default:
          this.mounts.get(message.mount)?.receive(message);
      }
    };
  }

  /** @internal */
  post(message: ToWorker): void {
    if (!this.closed) this.worker.postMessage(message);
  }

  private call<M extends Method>(
    method: M,
    params: Methods[M]["params"],
    signal?: AbortSignal,
  ): Promise<Methods[M]["result"]> {
    if (this.closed) return Promise.reject(new ClientError("closed", "closed"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      // Cancelled by the server, so the rejection is its `cancelled`,
      // also when the signal was aborted already.
      const cancel = () => this.post({ type: "cancel", id });
      const done = () => signal?.removeEventListener("abort", cancel);
      this.pending.set(id, { resolve: resolve as (result: unknown) => void, reject, done });
      this.post({ type: "request", id, method, params });
      if (signal?.aborted) cancel();
      else signal?.addEventListener("abort", cancel, { once: true });
    });
  }

  /** The server's catalog, or one method of it. */
  describe(): Promise<Catalog>;
  describe(method: string): Promise<Description>;
  describe(method?: string): Promise<Catalog | Description> {
    return this.call("describe", method === undefined ? {} : { method });
  }

  create(params: CreateParams = {}): Promise<Session> {
    return this.call("create", params);
  }

  /** Every session, `list` paged to its end. */
  async sessions(): Promise<Session[]> {
    const all: Session[] = [];
    for (let page = await this.list(); ; page = await this.list(page.next)) {
      all.push(...page.sessions);
      if (page.next === undefined) return all;
    }
  }

  /** One page of sessions, after the session id `after`. */
  list(after?: string): Promise<Page> {
    return this.call("list", after === undefined ? {} : { after });
  }

  inspect(session: string): Promise<Session> {
    return this.call("inspect", { session });
  }

  /** Hangs up the session's processes (SIGHUP). */
  async end(session: string): Promise<void> {
    await this.call("end", { session });
  }

  /** Sends `signal` (`HUP`, `INT`, `TERM`, `KILL`, ...) to the session's processes. */
  async signal(session: string, signal: string): Promise<void> {
    await this.call("signal", { session, signal });
  }

  /** Removes a session that is `exited` or `restored`. */
  async remove(session: string): Promise<void> {
    await this.call("remove", { session });
  }

  label(session: string, label: string): Promise<Session> {
    return this.call("label", { session, label });
  }

  /** Sets the size, the size policy, or both. */
  async resize(session: string, size: Omit<ResizeParams, "session">): Promise<void> {
    await this.call("resize", { session, ...size });
  }

  /** Text, keys or bytes to the session's program, with a wait armed
   * before they are written when `input.wait` is given; aborting
   * `options.signal` cancels that wait. */
  send(session: string, input: SendInput, options: { signal?: AbortSignal } = {}): Promise<Sent> {
    return this.call("send", { session, ...input }, options.signal);
  }

  /** Waits on the server until one of `until` holds, or the timeout,
   * which is a result. */
  wait(session: string, until: Until, options: WaitOptions = {}): Promise<Waited> {
    const { signal, after, timeout_ms } = options;
    return this.call(
      "wait",
      { session, until, ...(after !== undefined && { after }), ...(timeout_ms !== undefined && { timeout_ms }) },
      signal,
    );
  }

  /** The screen, or the scope and format asked for. */
  read(session: string, options: Omit<ReadParams, "session"> = {}): Promise<Read> {
    return this.call("read", { session, ...options });
  }

  /** The stream as text, from the offset `after`. */
  tail(session: string, after: number, options: { max_bytes?: number } = {}): Promise<Tail> {
    return this.call("tail", { session, after, ...options });
  }

  /** Marks the session's `done` and `error` records seen: all, or those named. */
  ack(session: string, recordIds?: string[]): Promise<Acked> {
    return this.call("ack", { session, ...(recordIds !== undefined && { record_ids: recordIds }) });
  }

  /** Ends another attachment, by its `channel` in the session's `attachments`. */
  async detach(session: string, channel: number, reason?: string): Promise<void> {
    await this.call("detach", { session, channel, ...(reason !== undefined && { reason }) });
  }

  /** The connection's link, at each change: `connecting` with its
   * attempt, `connected` (with no session), or `ended`, as a terminal's
   * `onLink` reports the connection-level changes. Requests need
   * `connected`. A listener added later gets the latest at once. */
  onLink(listener: (link: Link) => void): () => void {
    return this.linkListeners.add(listener);
  }

  /** Every session on the server, at each change; a listener added once
   * the list is known gets it at once. */
  onSessions(listener: (sessions: Session[]) => void): () => void {
    return this.sessionListeners.add(listener);
  }

  /** Each server event, in `seq` order, without loss across reconnects:
   * the ones missed while the connection was down are replayed, or an
   * `EventGap` says they cannot be. */
  onEvent(listener: (event: Event | EventGap) => void): () => void {
    this.eventListeners.add(listener);
    return () => void this.eventListeners.delete(listener);
  }

  /** Mounts a terminal on `options.canvas`, attached to `options.session`
   * if given, on this client's connection. */
  mount(options: MountOptions): Terminal {
    if (this.closed) throw new ClientError("closed", "closed");
    const id = this.nextId++;
    const host: MountHost = {
      send: (message) => this.post(message),
      disposed: () => void this.mounts.delete(id),
    };
    const mounted = mountOn(host, id, options);
    this.mounts.set(id, mounted);
    return mounted.terminal;
  }

  /** Disposes the terminals and terminates the worker, which ends the
   * connection; requests in flight reject with `closed`. */
  close(): void {
    if (this.closed) return;
    for (const { terminal } of [...this.mounts.values()]) terminal.dispose();
    this.closed = true;
    this.worker.onmessage = null;
    this.worker.terminate();
    for (const waiter of this.pending.values()) {
      waiter.done();
      waiter.reject(new ClientError("closed", "closed"));
    }
    this.pending.clear();
    this.sessionListeners.clear();
    this.eventListeners.clear();
    this.linkListeners.clear();
  }
}

function errorOf(failure: Failure): ClientError {
  return new ClientError(failure.kind, failure.message, failure.server);
}
