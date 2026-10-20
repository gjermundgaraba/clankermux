/**
 * The engine side of a client: the connection, the attachments and their
 * render state live in a worker; the page draws frames and reports input.
 * One worker serves one `Client` and every terminal mounted from it.
 * Imported as `@gjermundgaraba/clankermux/worker`, so a worker typechecked
 * without the DOM library does not see the page's types.
 */
import {
  type CellPoint,
  ENGINE,
  Engine,
  type Frame,
  KeyEncoder,
  type KeyInput,
  RenderState,
  SelectionGesture,
  type Terminal,
} from "../engine/index.ts";
import {
  type ErrorCode,
  type ErrorMessage,
  type Event,
  type Exit,
  FrameType,
  type Page,
  type Peer,
  type Profile,
  type Session,
  type Size,
  type Subscribed,
} from "../proto/index.ts";

import { type Action, Attachment } from "./attachment.ts";
import { ClientError, Connection, type ErrorKind, type Transport, hello } from "./connection.ts";
import { Osc52Scanner } from "./osc52.ts";

/** Why the link to the session ended. `closed`: the connection closed
 * (not a `CLOSED` frame, which the worker does not wait for when it
 * detaches); `removed`, `stopping` and `detached` (another client's
 * `detach`): the server's `CLOSED`; `exited`:
 * the session's program exited; `no_such_session` and `engine_mismatch`:
 * an `OPEN` refused; `error`: any other error, the server's or the
 * client's own. */
export type LinkEnd =
  | "closed"
  | "removed"
  | "stopping"
  | "detached"
  | "exited"
  | "no_such_session"
  | "engine_mismatch"
  | "error";

/** A terminal's link to its session, through the client's connection and
 * the terminal's attachment. */
export type Link =
  /** Opening a connection; `attempt` counts from 1 and starts again once
   * a server answers. */
  | { state: "connecting"; attempt: number }
  /** Connected, and showing `session` as `ATTACHED` described it (its
   * `status` may be `restored` or `exited`), or attached to none. */
  | { state: "connected"; session: Session | undefined }
  /** The link ended. `code` and `message` are the error's, when there is
   * one, and `message` the reason a `detach` gave; `exit` is the
   * program's, for `exited`. `retry`: the worker
   * reconnects or attaches again by itself, so `connecting` or
   * `connected` follows; otherwise nothing does until the page attaches. */
  | {
      state: "ended";
      reason: LinkEnd;
      code?: ErrorCode;
      message?: string;
      exit?: Exit;
      retry: boolean;
    };

/** A failed request, as it crosses to the page. */
export interface Failure {
  kind: ErrorKind;
  message: string;
  server?: ErrorMessage;
}

/** What a mounted terminal starts with. `active`: the user is looking at
 * the page (below). */
export interface MountInit {
  session: string | undefined;
  viewport: Size;
  profile: Profile;
  active: boolean;
  readOnly: boolean;
  ackOnFocus: boolean;
}

/** Messages from the page to the worker. */
export type ToWorker =
  | { type: "init"; wasm: string; url: string; client: Peer | undefined }
  /** A protocol request, answered by a `reply` with its `id`. */
  | { type: "request"; id: number; method: string; params: unknown }
  /** Cancels the pending `wait` (or `send` with `wait`) of request `id`. */
  | { type: "cancel"; id: number }
  /** The terminals, each by the page's number for it. */
  | ({ type: "mount"; mount: number } & MountInit)
  | { type: "unmount"; mount: number }
  | { type: "attach"; mount: number; session: string }
  | { type: "detach"; mount: number }
  | { type: "key"; mount: number; input: KeyInput }
  | { type: "paste"; mount: number; text: string }
  /** The mouse on the canvas, `at` in cells of the viewport, fractions
   * included; `time` in ms, for double and triple clicks. A release has
   * the page's number for its copy, which the answer carries. */
  | { type: "select"; mount: number; action: "press" | "drag"; at: CellPoint; time: number }
  | { type: "select"; mount: number; action: "release"; at: CellPoint; time: number; copy: number }
  /** Text as typed, which no key sent: a composition's, the emoji picker's. */
  | { type: "text"; mount: number; text: string }
  /** The wheel: `rows` through the history, up when negative. */
  | { type: "wheel"; mount: number; rows: number }
  /** The view through the history: `rows` from where it is, up when
   * negative, or back to the live screen. */
  | { type: "view"; mount: number; rows: number | "live" }
  /** The page's size and profile. Only an `active` page, one the user is
   * looking at, reports them to the session: under the `latest` policy
   * the server applies the latest report, so the session takes the size
   * of the page in use. */
  | { type: "viewport"; mount: number; size: Size; profile: Profile; active: boolean }
  /** The terminal has the keyboard, and its window the focus, or not. */
  | { type: "focus"; mount: number; focused: boolean }
  /** The page lost its drawing: send the whole screen again. */
  | { type: "repaint"; mount: number };

/** Messages from the worker to the page. */
export type ToPage =
  /** The engine loaded: the client is usable. */
  | { type: "ready" }
  /** The engine did not load; nothing else follows. */
  | { type: "failed"; message: string }
  | { type: "reply"; id: number; result?: unknown; error?: Failure }
  /** Every session on the server, at each change. */
  | { type: "sessions"; sessions: Session[] }
  | { type: "event"; event: Event }
  /** Events were lost: the server restarted, or more happened while the
   * connection was down than it retains. `sessions` was paged again. */
  | { type: "gap"; incarnation: string }
  /** The connection's own link: `connecting`, `connected` with no
   * session, or `ended`. */
  | { type: "connection"; link: Link }
  | { type: "status"; mount: number; text: string }
  | { type: "link"; mount: number; link: Link }
  | { type: "frame"; mount: number; frame: Frame }
  | { type: "clipboard"; mount: number; text: string }
  /** The selection's text, or undefined once there is none; with `copy`,
   * the answer to the release of that number, so the text (if any) goes
   * to the clipboard. Every release is answered. */
  | { type: "selection"; mount: number; text: string | undefined; copy?: number };

/** The worker's end of the page's messages: `self` in a dedicated worker. */
export interface WorkerPort {
  postMessage(message: ToPage): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
}

/** The keys that, with Shift, scroll the history: by pages, or to its
 * top or bottom. */
const SCROLL_KEYS: Record<string, number | "top" | "bottom"> = {
  PageUp: -1,
  PageDown: 1,
  Home: "top",
  End: "bottom",
};

/** Coalescing window for paints. */
const PAINT_MS = 16;
const BACKOFF_MIN_MS = 250;
const BACKOFF_MAX_MS = 5000;
/** Ending errors of an OPEN that a new OPEN would only get again. */
const PERMANENT = new Set<ErrorMessage["code"]>(["no_such_session", "engine_mismatch", "invalid_request"]);
/** The records `ack` marks seen. */
const ACKED = new Set(["done", "error"]);

/** Delays before trying again, doubling up to the maximum. */
class Backoff {
  private ms = BACKOFF_MIN_MS;

  next(): number {
    const delay = this.ms;
    this.ms = Math.min(this.ms * 2, BACKOFF_MAX_MS);
    return delay;
  }

  reset(): void {
    this.ms = BACKOFF_MIN_MS;
  }
}

/** A WebSocket as the client's message transport. */
class SocketTransport implements Transport {
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  private constructor(private readonly socket: WebSocket) {
    socket.onmessage = (e: MessageEvent<ArrayBuffer>) => this.onMessage?.(new Uint8Array(e.data));
    socket.onclose = () => this.onClose?.();
  }

  static connect(url: string): Promise<SocketTransport> {
    return new Promise((resolve, reject) => {
      const socket = new WebSocket(url);
      socket.binaryType = "arraybuffer";
      socket.onopen = () => resolve(new SocketTransport(socket));
      socket.onerror = () => reject(new Error(`cannot connect to ${url}`));
      socket.onclose = () => reject(new Error(`cannot connect to ${url}`));
    });
  }

  send(bytes: Uint8Array): void {
    this.socket.send(bytes);
  }

  close(): void {
    this.socket.close();
  }
}

function failureOf(err: unknown): Failure {
  return err instanceof ClientError
    ? { kind: err.kind, message: err.message, ...(err.server && { server: err.server }) }
    : { kind: "protocol", message: err instanceof Error ? err.message : String(err) };
}

/** The link ended with `error`: the server's, else the client's own. */
function endedWith(err: unknown, retry: boolean): Link {
  const server = err instanceof ClientError ? err.server : undefined;
  return server
    ? { state: "ended", reason: linkEnd(server.code), code: server.code, message: server.message, retry }
    : { state: "ended", reason: "error", message: err instanceof Error ? err.message : String(err), retry };
}

/**
 * The client: one connection, reconnected after a loss, its session list
 * and event stream, the page's requests, and the mounted terminals.
 */
class Hub {
  constructor(readonly post: (message: ToPage) => void) {}

  engine!: Engine;
  keys!: KeyEncoder;
  private url = "";
  private client: Peer | string = "clankermux-web";
  conn: Connection | undefined;
  /** The connection-level link, as a terminal mounted now first sees it:
   * `connecting`, `connected` (to no session), or `ended` for good. */
  private state: Link = { state: "connecting", attempt: 1 };
  /** The latest connection-level status text, for a terminal mounted later. */
  private statusText = "connecting";
  /** Connection attempts since a server last answered. */
  private attempt = 0;
  private readonly backoff = new Backoff();
  /** The connection ended for good with the server's refusal or error;
   * an `attach` connects again. */
  private refused = false;
  /** Frame channels, shared by the terminals of the one connection. */
  private channel = 0;
  readonly mounts = new Map<number, Mount>();
  private readonly sessions = new Map<string, Session>();
  /** The list is being paged: not shown to the page until it is whole. */
  private paging = false;
  /** Where the event stream is, to replay from after a reconnect. */
  private events: { incarnation: string; seq: number } | undefined;
  /** The page's pending requests: their connection and its request id. */
  private readonly requests = new Map<number, { conn: Connection; wire: number }>();

  async init(message: Extract<ToWorker, { type: "init" }>): Promise<void> {
    this.url = message.url;
    if (message.client) this.client = message.client;
    try {
      const wasm = await (await fetch(message.wasm)).arrayBuffer();
      this.engine = await Engine.load(wasm);
    } catch (err) {
      this.post({ type: "failed", message: `cannot load the engine: ${err}` });
      return;
    }
    this.keys = new KeyEncoder(this.engine);
    this.post({ type: "ready" });
    await this.connect();
  }

  /** A connection-level change, to the page and every terminal. A
   * retrying end leaves the connection `connecting` meanwhile. */
  private linkAll(link: Link): void {
    this.state = link.state === "ended" && link.retry ? { state: "connecting", attempt: this.attempt + 1 } : link;
    this.post({ type: "connection", link });
    for (const mount of this.mounts.values()) mount.link(link);
  }

  private statusAll(text: string): void {
    this.statusText = text;
    for (const mount of this.mounts.values()) mount.status(text);
  }

  /** The connection is up and subscribed: terminals may open channels. */
  live(): Connection | undefined {
    return this.state.state === "connected" ? this.conn : undefined;
  }

  /** The connection ended for good with `err`. */
  private refuse(err: unknown): void {
    this.refused = true;
    this.statusAll(err instanceof Error ? err.message : String(err));
    this.linkAll(endedWith(err, false));
  }

  private async connect(): Promise<void> {
    this.statusAll(this.events ? "reconnecting" : "connecting");
    this.linkAll({ state: "connecting", attempt: ++this.attempt });
    let conn: Connection;
    try {
      conn = await Connection.connect(await SocketTransport.connect(this.url), hello(this.client, ENGINE));
    } catch (err) {
      // A server that answers and refuses is not retried; a lost one is.
      if (err instanceof ClientError && (err.kind === "server" || err.kind === "protocol")) {
        this.refuse(err);
        return;
      }
      setTimeout(() => void this.connect(), this.backoff.next());
      return;
    }
    this.attempt = 0;
    this.backoff.reset();
    this.conn = conn;
    conn.onEnd = (err) => {
      if (this.conn !== conn) return;
      this.conn = undefined;
      if (err.kind === "server") {
        this.refuse(err);
      } else {
        this.linkAll({ state: "ended", reason: "closed", retry: true });
        setTimeout(() => void this.connect(), this.backoff.next());
      }
    };
    conn.onEvent = (event) => this.event(event);
    try {
      await this.subscribe(conn);
    } catch (err) {
      // A lost connection is reported through onEnd, which ran first; a
      // refused subscription ends this one for good.
      if (this.conn === conn) {
        this.conn = undefined;
        conn.close();
        this.refuse(err);
      }
      return;
    }
    this.state = { state: "connected", session: undefined };
    this.statusText = "connected";
    this.post({ type: "connection", link: this.state });
    for (const mount of this.mounts.values()) mount.connected();
  }

  /** Subscribes: from the first page on the first connection; after a
   * reconnect from the last event seen, replaying those missed, or, when
   * the server cannot (a gap), from the first page again. */
  private async subscribe(conn: Connection): Promise<void> {
    const incarnation = conn.welcome.incarnation;
    if (this.events) {
      const replay = await conn.call<Subscribed>("subscribe", {
        after: this.events.seq,
        incarnation: this.events.incarnation,
      });
      if (!replay.gap) return;
      this.events = { incarnation, seq: replay.latest };
      await this.page(conn, await conn.call<Page>("list", {}));
      this.post({ type: "gap", incarnation });
      for (const mount of this.mounts.values()) mount.recheck();
      return;
    }
    const first = await conn.call<Subscribed>("subscribe", {});
    this.events = { incarnation, seq: first.latest };
    await this.page(conn, { sessions: first.sessions ?? [], ...(first.next !== undefined && { next: first.next }) });
  }

  /** The session list from `page` on. Events arriving between pages are
   * applied to the same map: a later page cannot hold a session removed
   * before it was produced. The page sees the list once it is whole. */
  private async page(conn: Connection, page: Page): Promise<void> {
    this.sessions.clear();
    this.paging = true;
    try {
      for (;;) {
        for (const s of page.sessions) this.sessions.set(s.id, s);
        if (page.next === undefined) break;
        page = await conn.call<Page>("list", { after: page.next });
      }
    } finally {
      this.paging = false;
    }
    this.postSessions();
  }

  private event(event: Event): void {
    if (this.events) this.events.seq = event.seq;
    switch (event.event) {
      case "session_created":
      case "session_changed":
      case "session_exited":
        this.sessions.set(event.session.id, event.session);
        this.postSessions();
        for (const mount of this.mounts.values()) mount.changed(event.session);
        break;
      case "session_removed":
        this.sessions.delete(event.id);
        this.postSessions();
        break;
    }
    this.post({ type: "event", event });
  }

  session(id: string): Session | undefined {
    return this.sessions.get(id);
  }

  private postSessions(): void {
    if (!this.paging) this.post({ type: "sessions", sessions: [...this.sessions.values()] });
  }

  nextChannel(): number {
    return ++this.channel;
  }

  /** A protocol request from the page; its outcome goes back by `id`. */
  async request(id: number, method: string, params: unknown): Promise<void> {
    const conn = this.conn;
    if (!conn) {
      this.post({ type: "reply", id, error: { kind: "closed", message: "not connected" } });
      return;
    }
    const { id: wire, reply } = conn.request(method, params);
    this.requests.set(id, { conn, wire });
    try {
      this.post({ type: "reply", id, result: await reply });
    } catch (err) {
      this.post({ type: "reply", id, error: failureOf(err) });
    } finally {
      this.requests.delete(id);
    }
  }

  /** Cancels the page's request `id`, when it is still pending. Its own
   * reply is the server's `cancelled`. */
  cancel(id: number): void {
    const pending = this.requests.get(id);
    if (!pending || pending.conn !== this.conn) return;
    pending.conn.call("cancel", { id: pending.wire }).catch(() => {});
  }

  /** A terminal mounted after the connection ended for good connects
   * again, as an `attach` does; otherwise it first sees the connection as
   * it is now, as a terminal mounted at the start would have. */
  mount(id: number, init: MountInit): void {
    const mount = new Mount(this, id, init);
    this.mounts.set(id, mount);
    mount.status(this.statusText);
    if (this.refused) this.reattach(mount);
    else if (this.live()) mount.connected();
    else mount.link(this.state);
  }

  unmount(id: number): void {
    this.mounts.get(id)?.dispose();
    this.mounts.delete(id);
  }

  /** After the server refused or ended the connection, connects again;
   * otherwise opens `mount`'s session, or leaves it to the connection
   * coming up. */
  reattach(mount: Mount): void {
    if (this.refused) {
      this.refused = false;
      this.attempt = 0;
      void this.connect();
    } else {
      mount.open();
    }
  }
}

/** One mounted terminal: an attachment on the hub's connection, its render
 * state, selection, input and size. */
class Mount {
  private attachment: Attachment | undefined;
  private channel = 0;
  /** The connection the channel is open on. */
  private openOn: Connection | undefined;
  /** An `ack` is under way. */
  private acking = false;
  /** The session attached or being attached. */
  private session: string | undefined;
  private viewport: Size;
  private profile: Profile;
  /** The page is in use; only then are its size and profile reported. */
  private active: boolean;
  private readonly readOnly: boolean;
  private readonly ackOnFocus: boolean;
  /** The terminal has the keyboard and its window the focus. */
  private focused = false;
  /** Another client resized the session while this page was in use: its
   * next input reports its size again. */
  private reclaim = false;
  private render: RenderState;
  private readonly gesture: SelectionGesture;
  /** The page holds a selection's text. */
  private selected = false;
  /** Input in order: a paste goes as `send` requests, and keys typed
   * meanwhile wait behind it. `attach` starts a fresh chain; input queued
   * for an attachment that is gone is dropped, but a paste under way
   * finishes (cut short, it would leave the program inside a bracketed
   * paste). */
  private writes: Promise<void> = Promise.resolve();
  private readonly clipboard = new Osc52Scanner();
  private paintTimer: ReturnType<typeof setTimeout> | undefined;
  /** Re-attaches after a failure; reset by a screen. */
  private readonly backoff = new Backoff();
  private reopenTimer: ReturnType<typeof setTimeout> | undefined;
  private disposed = false;

  constructor(
    private readonly hub: Hub,
    readonly id: number,
    init: MountInit,
  ) {
    this.session = init.session;
    this.viewport = init.viewport;
    this.profile = init.profile;
    this.active = init.active;
    this.readOnly = init.readOnly;
    this.ackOnFocus = init.ackOnFocus;
    this.render = new RenderState(hub.engine);
    this.gesture = new SelectionGesture(hub.engine);
  }

  private post(message: ToPage): void {
    if (!this.disposed) this.hub.post(message);
  }

  link(link: Link): void {
    this.post({ type: "link", mount: this.id, link });
  }

  status(text: string): void {
    this.post({ type: "status", mount: this.id, text });
  }

  /** The hub's connection is up: attach the session, if there is one. */
  connected(): void {
    if (this.session !== undefined) {
      this.open();
    } else {
      this.status("no session");
      this.link({ state: "connected", session: undefined });
    }
  }

  /** Opens a channel for `this.session`, resuming when the attachment can. */
  open(): void {
    const conn = this.hub.live();
    if (!conn || this.session === undefined || this.disposed || this.openOn === conn) return;
    if (!this.attachment) this.attachment = new Attachment(this.hub.engine);
    const attachment = this.attachment;
    attachment.openOn(conn.welcome);
    const channel = this.hub.nextChannel();
    this.channel = channel;
    this.openOn = conn;
    const resume = attachment.resume();
    // An OPEN with a viewport or profile is a report applied at once.
    const open = {
      session: this.session,
      ...(this.readOnly && { read_only: true }),
      ...(this.active && { viewport: this.viewport, profile: this.profile }),
      ...(resume && { resume }),
    };
    try {
      conn.open(channel, open, (frame) => {
        if (this.attachment !== attachment) return;
        let actions: Action[];
        try {
          actions = attachment.handle(frame);
        } catch (err) {
          // A lost place in the stream: CLOSE and OPEN again, afresh. Any
          // other fault would only repeat.
          this.status(String(err));
          const desync = err instanceof ClientError && err.kind === "desync";
          this.link(endedWith(err, desync));
          if (desync) this.reopen();
          else this.giveUp();
          return;
        }
        if (frame.header.type === FrameType.Attached && actions.length === 0) {
          // Resumed: the terminal shows the stream already, and an exit
          // still to come arrives as EXITED. A snapshot's screen comes
          // later, as an action.
          if (attachment.resume()) {
            this.status("resumed");
            this.link({ state: "connected", session: attachment.session });
          }
        }
        for (const action of actions) this.apply(action);
      });
    } catch (err) {
      this.status(String(err));
    }
  }

  /** The screen of `session` is shown: `connected`, and at once `ended
   * exited` when its program has exited already. */
  private attached(session: Session | undefined): void {
    this.link({ state: "connected", session });
    if (session?.status === "exited") {
      this.link({ state: "ended", reason: "exited", ...(session.exit && { exit: session.exit }), retry: false });
    }
  }

  /** Detaches for good: a reconnect does not try the session again. */
  private giveUp(): void {
    this.close();
    this.session = undefined;
  }

  /** Detaches and attaches the same session again after a backoff. */
  private reopen(): void {
    this.close();
    this.reopenTimer ??= setTimeout(() => {
      this.reopenTimer = undefined;
      if (this.session !== undefined && !this.attachment) this.open();
    }, this.backoff.next());
  }

  private apply(action: Action): void {
    switch (action.type) {
      case "screen": {
        this.backoff.reset();
        this.forgetSelection();
        this.clipboard.reset();
        this.applyProfile(this.attachment?.term());
        this.paint();
        const session = this.attachment?.session;
        const text =
          session?.status === "restored"
            ? "restored from a previous server; no process"
            : session?.exit
              ? `exited: ${describe(session.exit)}`
              : "attached";
        this.status(text);
        this.attached(session);
        if (session) this.changed(session);
        break;
      }
      case "output":
        for (const write of this.clipboard.feed(action.bytes)) {
          // Only the clipboard selection; primary and secondary have no
          // browser equivalent.
          if (write.selection === "" || write.selection.includes("c")) {
            this.post({ type: "clipboard", mount: this.id, text: write.text });
          }
        }
        this.paintTimer ??= setTimeout(() => this.paint(), PAINT_MS);
        break;
      case "resized":
        // Another client's size, while this page kept its focus (the user
        // went to another machine): taken back at this page's next input.
        if (this.active && (action.cols !== this.viewport.cols || action.rows !== this.viewport.rows)) {
          this.reclaim = true;
        }
        this.paint();
        break;
      case "exited":
        this.paint();
        this.status(`exited: ${describe(action.exit)}`);
        this.link({ state: "ended", reason: "exited", exit: action.exit, retry: false });
        break;
      case "gap":
        this.clipboard.reset();
        this.forgetSelection();
        this.status("resynchronising");
        break;
      case "closed":
        this.status(`closed: ${action.reason}`);
        // `stopping` is followed by the connection's close, and the
        // reconnect attaches again; a removed session is gone for good,
        // and a detached page does not take its session back.
        this.link({
          state: "ended",
          reason: action.reason,
          ...(action.message !== undefined && { message: action.message }),
          retry: action.reason === "stopping",
        });
        if (action.reason === "removed" || action.reason === "detached") this.giveUp();
        break;
      case "error": {
        this.status(`${action.error.code}: ${action.error.message}`);
        // An error ended the attachment: attach again after a backoff
        // (`too_slow`, a snapshot that could not be taken), unless a new
        // OPEN would fail the same way.
        if (!this.attachment?.closed()) break;
        const { code, message } = action.error;
        const retry = !PERMANENT.has(code);
        this.link({ state: "ended", reason: linkEnd(code), code, message, retry });
        if (retry) this.reopen();
        else this.giveUp();
        break;
      }
    }
  }

  /** The page's colours on the local terminal, as the server has them on
   * its own: the stream does not carry default colours. */
  private applyProfile(term: Terminal | undefined): void {
    if (!term) return;
    for (const [kind, value] of [
      ["foreground", this.profile.fg],
      ["background", this.profile.bg],
      ["cursor", this.profile.cursor],
    ] as const) {
      if (value !== undefined) term.setColor(kind, parseInt(value.slice(1), 16));
    }
  }

  repaint(): void {
    const term = this.attachment?.term();
    if (!term) return;
    // Switching the render state to a fresh terminal makes the next
    // update full; so does a resize. A full repaint of the same terminal
    // comes from a clean render state.
    this.render.free();
    this.render = new RenderState(this.hub.engine);
    this.paint();
  }

  private paint(): void {
    if (this.paintTimer !== undefined) {
      clearTimeout(this.paintTimer);
      this.paintTimer = undefined;
    }
    const term = this.attachment?.term();
    if (!term || this.disposed) return;
    this.post({ type: "frame", mount: this.id, frame: this.render.update(term) });
    this.render.clean();
    // A switch to the alternate screen or a full reset took the selection.
    if (this.selected && !term.selected) this.unselected();
  }

  /** The mouse: a press clears the selection, or selects a word or a line
   * at once; a drag selects; the release copies what is selected. */
  select(message: Extract<ToWorker, { type: "select" }>): void {
    const { at, time } = message;
    const term = this.attachment?.term();
    if (!term) {
      // Nothing to select: the page's copy waits for the answer even so.
      if (message.action === "release") {
        this.post({ type: "selection", mount: this.id, text: undefined, copy: message.copy });
      }
      return;
    }
    switch (message.action) {
      case "press":
        this.claim();
        if (this.selected) this.unselected();
        term.select(this.gesture.press(term, at, time));
        break;
      case "drag": {
        const selection = this.gesture.drag(term, at);
        if (selection) term.select(selection);
        break;
      }
      case "release": {
        this.gesture.release(term, at);
        // Answered either way: the page's copy waits for it. Blanks only
        // (trimmed to nothing) are no text to copy.
        const text = term.selectionText() || undefined;
        this.selected = text !== undefined;
        this.post({ type: "selection", mount: this.id, text, copy: message.copy });
        break;
      }
    }
    this.paint();
  }

  /** Input clears the selection. */
  private unselect(): void {
    const term = this.attachment?.term();
    if (!term?.selected) return;
    term.select(undefined);
    this.paint();
    if (this.selected) this.unselected();
  }

  private unselected(): void {
    this.selected = false;
    this.post({ type: "selection", mount: this.id, text: undefined });
  }

  /** The terminal goes, and its selection and the gesture's anchor with it. */
  private forgetSelection(term?: Terminal): void {
    this.gesture.forget(term);
    if (this.selected) this.unselected();
  }

  /** Closes the current attachment's channel and forgets it. An
   * attachment the server ended needs no CLOSE; one whose stream ended
   * (EXITED) does. */
  private close(): void {
    if (!this.attachment) return;
    // Still alive: the gesture's anchor is released in it.
    this.forgetSelection(this.attachment.term());
    const conn = this.hub.conn;
    if (conn) {
      conn.release(this.channel);
      if (!this.attachment.closed()) {
        try {
          conn.send(FrameType.Close, this.channel, new Uint8Array());
        } catch {
          // The connection is gone; nothing to close.
        }
      }
    }
    this.attachment.free();
    this.attachment = undefined;
    this.openOn = undefined;
  }

  /** Detaches the current session and attaches `session`; after the
   * server refused or ended the connection, connects again first. */
  attach(session: string): void {
    this.close();
    this.writes = Promise.resolve();
    this.session = session;
    this.hub.reattach(this);
  }

  /** Detaches, and stays attached to none. */
  detach(): void {
    const had = this.session !== undefined;
    this.giveUp();
    this.writes = Promise.resolve();
    if (had && this.hub.conn) {
      this.status("no session");
      this.link({ state: "connected", session: undefined });
    }
  }

  dispose(): void {
    this.close();
    this.disposed = true;
    if (this.paintTimer !== undefined) clearTimeout(this.paintTimer);
    if (this.reopenTimer !== undefined) clearTimeout(this.reopenTimer);
    this.render.free();
    this.gesture.forget(undefined);
  }

  /** Input for the session, dropped unless attached on a live connection,
   * and for a read-only terminal. Keys go as INPUT; a paste goes as
   * `send` requests, each answered once the PTY took it, since INPUT
   * frames are not acknowledged and a paste would overflow the input
   * queue. This terminal's input is sent in order, so none of it lands
   * inside a paste. */
  private input(bytes: Uint8Array, paste: boolean): void {
    const conn = this.hub.conn;
    const attachment = this.attachment;
    const session = this.session;
    const channel = this.channel;
    if (bytes.length === 0 || this.readOnly || !conn || !attachment || attachment.ended() || session === undefined) {
      return;
    }
    this.claim();
    this.live();
    this.unselect();
    // Each request's base64 fits the control body with room for the rest,
    // and its bytes fit the input queue (at least one byte, so the loop
    // moves on an absurd limit).
    const { control_body, input_queue } = conn.welcome.limits;
    const chunk = Math.max(1, Math.min(Math.floor((control_body - 1024) / 4) * 3, input_queue));
    this.writes = this.writes.then(async () => {
      if (this.attachment !== attachment) return;
      try {
        if (!paste) {
          conn.send(FrameType.Input, channel, bytes);
          return;
        }
        for (let at = 0; at < bytes.length; at += chunk) {
          await conn.call("send", { session, data: base64(bytes.subarray(at, at + chunk)) });
        }
      } catch (err) {
        // A connection loss is reported through onEnd; a refused chunk
        // ends the paste there.
        if (conn === this.hub.conn) this.status(String(err));
      }
    });
  }

  key(input: KeyInput): void {
    const term = this.attachment?.term();
    if (!term) return;
    // Shift+PageUp, PageDown, Home and End scroll the history, press and
    // release; on the alternate screen, which has none, they are keys.
    const scroll = SCROLL_KEYS[input.code];
    if (scroll && input.shift && !input.ctrl && !input.alt && !input.meta && !term.alternate) {
      if (input.action === "release") return;
      term.scroll(typeof scroll === "number" ? scroll * term.rows : scroll);
      this.paint();
      return;
    }
    this.input(this.hub.keys.encode(term, input), false);
  }

  /** The wheel. On the alternate screen, with alternate scroll (mode
   * 1007, on by default), it is the arrow keys, as a full-screen program
   * such as `less` expects; without it, nothing. */
  wheel(rows: number): void {
    const term = this.attachment?.term();
    if (!term || rows === 0) return;
    this.claim();
    if (!term.alternate) {
      term.scroll(rows);
      this.paint();
      return;
    }
    if (!term.mode(1007)) return;
    const code = rows < 0 ? "ArrowUp" : "ArrowDown";
    const arrow = this.hub.keys.encode(term, { code, key: code, action: "press", shift: false, ctrl: false, alt: false, meta: false });
    const bytes = new Uint8Array(arrow.length * Math.abs(rows));
    for (let i = 0; i < Math.abs(rows); i++) bytes.set(arrow, i * arrow.length);
    this.input(bytes, false);
  }

  /** The view through the history, as the page asks: by rows, or back to
   * the live screen. The alternate screen has no history. */
  view(rows: number | "live"): void {
    const term = this.attachment?.term();
    if (!term || term.alternate) return;
    if (rows === "live") this.live();
    else if (rows !== 0) {
      term.scroll(rows);
      this.paint();
    }
  }

  /** Back to the live screen before input, as any terminal does. */
  private live(): void {
    const term = this.attachment?.term();
    if (!term) return;
    const { total, offset, len } = term.scrollbar;
    if (offset + len >= total) return;
    term.scroll("bottom");
    this.paint();
  }

  /** Text no key sent, as UTF-8: a composition's, the emoji picker's. */
  text(text: string): void {
    if (!this.attachment?.term()) return;
    this.input(new TextEncoder().encode(text), false);
  }

  paste(text: string): void {
    const term = this.attachment?.term();
    if (!term) return;
    this.input(this.hub.engine.encodePaste(text, term.mode(2004)), true);
  }

  /** Keeps the page's size and colours (for the local terminal), and
   * reports them to the session while the page is active. A report
   * repeats the last one when the page becomes active again, so the
   * session takes its size back from another client. */
  setViewport(size: Size, profile: Profile, active: boolean): void {
    this.viewport = size;
    this.profile = profile;
    this.active = active;
    const attachment = this.attachment;
    if (!this.hub.conn || !attachment || attachment.ended()) return;
    this.applyProfile(attachment.term());
    this.paint();
    if (active) this.report();
  }

  private report(): void {
    this.reclaim = false;
    try {
      this.hub.conn?.sendJson(FrameType.Viewport, this.channel, { ...this.viewport, profile: this.profile });
    } catch {
      // Reported through onEnd.
    }
  }

  /** Input from the page in use: its size again if another client took it. */
  private claim(): void {
    if (this.reclaim && this.active && this.attachment && !this.attachment.ended()) this.report();
  }

  focus(focused: boolean): void {
    this.focused = focused;
    this.recheck();
  }

  /** Looks at this terminal's session again, as `changed` does. */
  recheck(): void {
    const session = this.session === undefined ? undefined : this.hub.session(this.session);
    if (session) this.changed(session);
  }

  /** A session changed: when it is this terminal's, its screen is shown,
   * the page opted in and the terminal has the focus, the `done` and
   * `error` records that want the user are marked seen. */
  changed(session: Session): void {
    if (!this.ackOnFocus || !this.focused || this.acking || session.id !== this.session) return;
    if (!this.attachment?.term() || !ACKED.has(session.attention?.state ?? "")) return;
    const conn = this.hub.live();
    if (!conn) return;
    // One at a time; a record that came meanwhile is looked at once the
    // ack is answered (not after a refusal, which would only repeat).
    this.acking = true;
    conn.call("ack", { session: session.id }).then(
      () => {
        this.acking = false;
        this.recheck();
      },
      () => (this.acking = false),
    );
  }
}

/** The reason an error code ends the link with. */
function linkEnd(code: ErrorCode): LinkEnd {
  return code === "no_such_session" || code === "engine_mismatch" ? code : "error";
}

function describe(exit: Exit): string {
  return "code" in exit ? `code ${exit.code}` : `signal ${exit.signal}`;
}

/** Serves a page's `connect` on `port`: the worker module the page starts
 * calls `runWorker(self)`. */
export function runWorker(port: WorkerPort): void {
  const hub = new Hub((message) => port.postMessage(message));
  port.onmessage = (e) => {
    const message = e.data;
    switch (message.type) {
      case "init":
        void hub.init(message);
        return;
      case "request":
        void hub.request(message.id, message.method, message.params);
        return;
      case "cancel":
        hub.cancel(message.id);
        return;
      case "mount": {
        const { type: _, mount, ...init } = message;
        hub.mount(mount, init);
        return;
      }
      case "unmount":
        hub.unmount(message.mount);
        return;
    }
    const mount = hub.mounts.get(message.mount);
    if (!mount) return;
    switch (message.type) {
      case "attach":
        mount.attach(message.session);
        break;
      case "detach":
        mount.detach();
        break;
      case "key":
        mount.key(message.input);
        break;
      case "paste":
        mount.paste(message.text);
        break;
      case "text":
        mount.text(message.text);
        break;
      case "select":
        mount.select(message);
        break;
      case "wheel":
        mount.wheel(message.rows);
        break;
      case "view":
        mount.view(message.rows);
        break;
      case "viewport":
        mount.setViewport(message.size, message.profile, message.active);
        break;
      case "focus":
        mount.focus(message.focused);
        break;
      case "repaint":
        mount.repaint();
        break;
    }
  };
}

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let at = 0; at < bytes.length; at += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  }
  return btoa(binary);
}
