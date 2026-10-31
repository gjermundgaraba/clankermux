import {
  type CellPoint,
  ENGINE,
  Engine,
  type PackedFrame,
  KeyEncoder,
  type KeyInput,
  RenderState,
  SelectionGesture,
  type Terminal,
} from "../engine/index.ts";
import {
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
  type Welcome,
} from "../proto/index.ts";

import { type Action, Attachment } from "./attachment.ts";
import { ClientError, Connection, type ErrorKind, type Transport, hello } from "./connection.ts";
import {
  type Line,
  type Link,
  type MountEvent,
  type MountSnapshot,
  type Open,
  endedWith,
  lineOf,
  linkOf,
  mountMachine,
} from "./link.ts";
import { Osc52Scanner } from "./osc52.ts";

export type { Link, LinkEnd } from "./link.ts";

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
  /** Whether the document is visible, independently of focus. */
  visible: boolean;
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
  /** Visibility is independent of focus: visible, unfocused pages still paint. */
  | { type: "visibility"; mount: number; visible: boolean }
  /** The page painted its one outstanding frame, or discarded it while hidden. */
  | { type: "painted"; mount: number }
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
  | { type: "frame"; mount: number; frame: PackedFrame }
  | { type: "clipboard"; mount: number; text: string }
  /** The selection's text, or undefined once there is none; with `copy`,
   * the answer to the release of that number, so the text (if any) goes
   * to the clipboard. Every release is answered. */
  | { type: "selection"; mount: number; text: string | undefined; copy?: number };

/** The worker's end of the page's messages: `self` in a dedicated worker. */
export interface WorkerPort {
  postMessage(message: ToPage, options?: StructuredSerializeOptions): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
}

const SCROLL_KEYS: Record<string, number | "top" | "bottom"> = {
  PageUp: -1,
  PageDown: 1,
  Home: "top",
  End: "bottom",
};

const PAINT_COALESCE_MS = 16;
const BACKOFF_MIN_MS = 250;
const BACKOFF_MAX_MS = 5000;
const ACKABLE_ATTENTION_STATES = new Set(["done", "error"]);
const ALTERNATE_SCROLL_MODE = 1007;
const SEND_REQUEST_OVERHEAD_BYTES = 1024;

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

function refusedByServer(err: unknown): boolean {
  return err instanceof ClientError && (err.kind === "server" || err.kind === "protocol");
}

function endedTheConnection(err: unknown): boolean {
  return err instanceof ClientError && err.server?.id === undefined;
}

function pasteChunkSize(limits: Welcome["limits"]): number {
  const bytesFittingAsBase64 = Math.floor((limits.control_body - SEND_REQUEST_OVERHEAD_BYTES) / 4) * 3;
  return Math.max(1, Math.min(bytesFittingAsBase64, limits.input_queue));
}

class InputQueue {
  private tail: Promise<void> = Promise.resolve();

  stopWaitingOnSendsUnderWay(): void {
    this.tail = Promise.resolve();
  }

  enqueueUnlessDetached(attached: () => boolean, send: () => Promise<void>): void {
    this.tail = this.tail.then(() => (attached() ? send() : undefined));
  }
}

function failureOf(err: unknown): Failure {
  return err instanceof ClientError
    ? { kind: err.kind, message: err.message, ...(err.server && { server: err.server }) }
    : { kind: "protocol", message: err instanceof Error ? err.message : String(err) };
}

class Hub {
  constructor(readonly post: WorkerPort["postMessage"]) {}

  engine!: Engine;
  keys!: KeyEncoder;
  private url = "";
  private client: Peer | string = "clankermux-web";
  conn: Connection | undefined;
  private connectionLink: Link = { state: "connecting", attempt: 1 };
  private statusText = "connecting";
  private attempt = 0;
  private readonly backoff = new Backoff();
  private channel = 0;
  readonly mounts = new Map<number, Mount>();
  private readonly sessions = new Map<string, Session>();
  private paging = false;
  private eventCursor: { incarnation: string; seq: number } | undefined;
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

  private linkAll(link: Link): void {
    this.connectionLink = link.state === "ended" && link.retry ? { state: "connecting", attempt: this.attempt + 1 } : link;
    this.post({ type: "connection", link });
    for (const mount of this.mounts.values()) mount.link(link);
  }

  private statusAll(text: string): void {
    this.statusText = text;
    for (const mount of this.mounts.values()) mount.status(text);
  }

  live(): Connection | undefined {
    return this.connectionLink.state === "connected" ? this.conn : undefined;
  }

  private refuse(err: unknown): void {
    this.statusAll(err instanceof Error ? err.message : String(err));
    this.linkAll(endedWith(err, false));
  }

  private async connect(): Promise<void> {
    this.statusAll(this.eventCursor ? "reconnecting" : "connecting");
    this.linkAll({ state: "connecting", attempt: ++this.attempt });
    let conn: Connection;
    try {
      conn = await Connection.connect(await SocketTransport.connect(this.url), hello(this.client, ENGINE));
    } catch (err) {
      if (refusedByServer(err)) {
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
      for (const mount of this.mounts.values()) mount.disconnected();
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
      if (endedTheConnection(err)) return;
      this.conn = undefined;
      conn.close();
      this.refuse(err);
      return;
    }
    this.connectionLink = { state: "connected", session: undefined };
    this.statusText = "connected";
    this.post({ type: "connection", link: this.connectionLink });
    for (const mount of this.mounts.values()) mount.connected(conn);
  }

  private async subscribe(conn: Connection): Promise<void> {
    const incarnation = conn.welcome.incarnation;
    if (this.eventCursor) {
      const replay = await conn.call<Subscribed>("subscribe", {
        after: this.eventCursor.seq,
        incarnation: this.eventCursor.incarnation,
      });
      if (!replay.gap) return;
      this.eventCursor = { incarnation, seq: replay.latest };
      await this.page(conn, await conn.call<Page>("list", {}));
      this.post({ type: "gap", incarnation });
      for (const mount of this.mounts.values()) mount.recheck();
      return;
    }
    const first = await conn.call<Subscribed>("subscribe", {});
    this.eventCursor = { incarnation, seq: first.latest };
    await this.page(conn, { sessions: first.sessions ?? [], ...(first.next !== undefined && { next: first.next }) });
  }

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
    if (this.eventCursor) this.eventCursor.seq = event.seq;
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

  cancel(id: number): void {
    const pending = this.requests.get(id);
    if (!pending || pending.conn !== this.conn) return;
    pending.conn.call("cancel", { id: pending.wire }).catch(() => {});
  }

  mount(id: number, init: MountInit): void {
    const mount = new Mount(this, id, init);
    this.mounts.set(id, mount);
    mount.status(this.statusText);
    const conn = this.live();
    if (conn) mount.connected(conn);
    else if (this.connectionLink.state !== "ended") mount.link(this.connectionLink);
    this.reconnectIfEnded();
  }

  unmount(id: number): void {
    this.mounts.get(id)?.dispose();
    this.mounts.delete(id);
  }

  reconnectIfEnded(): void {
    if (this.connectionLink.state !== "ended") return;
    this.attempt = 0;
    void this.connect();
  }
}

class Mount {
  private snapshot: MountSnapshot = mountMachine.initialState;
  private reopenTimer: ReturnType<typeof setTimeout> | undefined;
  private acking = false;
  private viewport: Size;
  private profile: Profile;
  private active: boolean;
  private visible: boolean;
  private readonly readOnly: boolean;
  private readonly ackOnFocus: boolean;
  private focused = false;
  private sizeTakenByOtherClient = false;
  private render: RenderState;
  private readonly gesture: SelectionGesture;
  private selected = false;
  private readonly inputQueue = new InputQueue();
  private readonly clipboard = new Osc52Scanner();
  private paintTimer: ReturnType<typeof setTimeout> | undefined;
  private paintPending = false;
  private frameInFlight = false;
  private fullNext = false;
  private readonly backoff = new Backoff();

  constructor(
    private readonly hub: Hub,
    readonly id: number,
    init: MountInit,
  ) {
    this.viewport = init.viewport;
    this.profile = init.profile;
    this.active = init.active;
    this.visible = init.visible;
    this.readOnly = init.readOnly;
    this.ackOnFocus = init.ackOnFocus;
    this.render = new RenderState(hub.engine);
    this.gesture = new SelectionGesture(hub.engine);
    if (init.session !== undefined) this.step({ type: "page.attach", session: init.session, open: undefined });
  }

  private get attachment(): Attachment | undefined {
    return this.snapshot.context.attachment;
  }

  private get session(): string | undefined {
    return this.snapshot.context.session;
  }

  private post(message: ToPage, options?: StructuredSerializeOptions): void {
    if (this.snapshot.value !== "disposed") this.hub.post(message, options);
  }

  link(link: Link): void {
    this.post({ type: "link", mount: this.id, link });
  }

  status(text: string): void {
    this.post({ type: "status", mount: this.id, text });
  }

  connected(conn: Connection): void {
    this.step({ type: "hub.live", open: this.offer(conn) });
  }

  disconnected(): void {
    this.step({ type: "hub.down" });
  }

  private offer(conn: Connection): Open {
    return { conn, channel: this.hub.nextChannel(), fresh: new Attachment(this.hub.engine) };
  }

  private offerIfLive(): Open | undefined {
    const conn = this.hub.live();
    return conn && this.offer(conn);
  }

  private step(event: MountEvent): void {
    const prev = this.snapshot;
    const [next] = mountMachine.transition(prev, event);
    if (next === prev) return;
    const { attachment } = prev.context;
    if (attachment && attachment !== next.context.attachment) this.close(attachment, lineOf(prev));
    if (prev.value === "backoff") clearTimeout(this.reopenTimer);
    this.snapshot = next;
    if (next.value === "opening") this.open(next.context);
    if (next.value === "backoff") {
      this.reopenTimer = setTimeout(() => this.step({ type: "reopen.due", open: this.offerIfLive() }), this.backoff.next());
    }
    const link = linkOf(next);
    if (!link) return;
    if (link.state === "connected" && link.session === undefined) this.status("no session");
    this.link(link);
  }

  private open({ session, attachment, channel, conn }: Line): void {
    attachment.openOn(conn.welcome);
    const resume = attachment.resume();
    const open = {
      session,
      ...(this.readOnly && { read_only: true }),
      ...(this.active && { viewport: this.viewport, profile: this.profile }),
      ...(resume && { resume }),
    };
    conn.open(channel, open, (frame) => {
      if (this.attachment !== attachment) return;
      let actions: Action[];
      try {
        actions = attachment.handle(frame);
      } catch (err) {
        this.status(String(err));
        this.step({ type: "attachment.threw", error: err });
        return;
      }
      for (const action of actions) this.apply(action);
    });
  }

  private close(attachment: Attachment, line: Line | undefined): void {
    this.forgetSelection(attachment.term());
    if (line) {
      line.conn.release(line.channel);
      if (!attachment.closed()) line.conn.send(FrameType.Close, line.channel, new Uint8Array());
    }
    attachment.free();
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
        this.step({ type: "frame.screen", session });
        if (session?.status === "exited") this.step({ type: "frame.exited", exit: session.exit });
        if (session) this.changed(session);
        break;
      }
      case "resumed":
        this.status("resumed");
        this.step({ type: "frame.resumed", session: this.attachment?.session });
        break;
      case "output":
        if (this.selected && !this.attachment?.term()?.selected) this.unselected();
        for (const write of this.clipboard.feed(action.bytes)) {
          if (write.toClipboard) this.post({ type: "clipboard", mount: this.id, text: write.text });
        }
        this.paintPending = true;
        if (this.visible && !this.frameInFlight) this.paintTimer ??= setTimeout(() => this.paint(), PAINT_COALESCE_MS);
        break;
      case "resized":
        if (this.active && (action.cols !== this.viewport.cols || action.rows !== this.viewport.rows)) {
          this.sizeTakenByOtherClient = true;
        }
        this.paint();
        break;
      case "exited":
        this.paint();
        this.status(`exited: ${describe(action.exit)}`);
        this.step({ type: "frame.exited", exit: action.exit });
        break;
      case "gap":
        this.clipboard.reset();
        this.forgetSelection();
        this.status("resynchronising");
        break;
      case "closed":
        this.status(`closed: ${action.reason}`);
        this.step({ type: "frame.closed", reason: action.reason, message: action.message });
        break;
      case "error":
        this.status(`${action.error.code}: ${action.error.message}`);
        this.step({ type: "frame.error", error: action.error, endsAttachment: this.attachment?.closed() === true });
        break;
    }
  }

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
    this.fullNext = true;
    this.paint();
  }

  visibility(visible: boolean): void {
    if (this.visible === visible) return;
    this.visible = visible;
    if (visible) this.repaint();
    else if (this.paintTimer !== undefined) {
      clearTimeout(this.paintTimer);
      this.paintTimer = undefined;
    }
  }

  painted(): void {
    if (!this.frameInFlight) return;
    this.frameInFlight = false;
    if (this.paintPending) this.paint();
  }

  private paint(): void {
    if (this.paintTimer !== undefined) {
      clearTimeout(this.paintTimer);
      this.paintTimer = undefined;
    }
    this.paintPending = true;
    const term = this.attachment?.term();
    if (!term || !this.visible || this.frameInFlight) return;
    if (this.fullNext) {
      this.render.free();
      this.render = new RenderState(this.hub.engine);
      this.fullNext = false;
    }
    const frame = this.render.takePackedFrame(term);
    this.paintPending = false;
    this.frameInFlight = true;
    this.post({ type: "frame", mount: this.id, frame }, { transfer: frame.lines.map((row) => row.cells.data.buffer) });
    if (this.selected && !term.selected) this.unselected();
  }

  select(message: Extract<ToWorker, { type: "select" }>): void {
    const { at, time } = message;
    const term = this.attachment?.term();
    if (!term) {
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
        const text = term.selectionText() || undefined;
        this.selected = text !== undefined;
        this.post({ type: "selection", mount: this.id, text, copy: message.copy });
        break;
      }
    }
    this.paint();
  }

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

  private forgetSelection(term?: Terminal): void {
    this.gesture.forget(term);
    if (this.selected) this.unselected();
  }

  attach(session: string): void {
    this.step({ type: "page.attach", session, open: this.offerIfLive() });
    this.inputQueue.stopWaitingOnSendsUnderWay();
    this.hub.reconnectIfEnded();
  }

  detach(): void {
    this.step({ type: "page.detach", live: this.hub.live() !== undefined });
    this.inputQueue.stopWaitingOnSendsUnderWay();
  }

  dispose(): void {
    this.step({ type: "page.unmount" });
    if (this.paintTimer !== undefined) clearTimeout(this.paintTimer);
    this.render.free();
    this.gesture.forget(undefined);
  }

  private sendInput(bytes: Uint8Array): void {
    const line = lineOf(this.snapshot);
    if (!line) return;
    this.queueInput(bytes, line, async () => {
      line.conn.send(FrameType.Input, line.channel, bytes);
    });
  }

  private sendPaste(bytes: Uint8Array): void {
    const { conn } = this.hub;
    const { session, attachment } = this.snapshot.context;
    if (!conn || session === undefined || !attachment) return;
    this.queueInput(bytes, { conn, attachment }, async () => {
      const chunk = pasteChunkSize(conn.welcome.limits);
      for (let at = 0; at < bytes.length; at += chunk) {
        await conn.call("send", { session, data: base64(bytes.subarray(at, at + chunk)) });
      }
    });
  }

  private queueInput(
    bytes: Uint8Array,
    target: { conn: Connection; attachment: Attachment },
    deliver: () => Promise<void>,
  ): void {
    if (bytes.length === 0 || this.readOnly || target.attachment.ended()) return;
    this.claim();
    this.backToLive();
    this.unselect();
    this.inputQueue.enqueueUnlessDetached(
      () => this.attachment === target.attachment,
      async () => {
        try {
          await deliver();
        } catch (err) {
          if (target.conn === this.hub.conn) this.status(String(err));
        }
      },
    );
  }

  key(input: KeyInput): void {
    const term = this.attachment?.term();
    if (!term) return;
    const scroll = SCROLL_KEYS[input.code];
    if (scroll && input.shift && !input.ctrl && !input.alt && !input.meta && !term.alternate) {
      if (input.action === "release") return;
      term.scroll(typeof scroll === "number" ? scroll * term.rows : scroll);
      this.paint();
      return;
    }
    this.sendInput(this.hub.keys.encode(term, input));
  }

  wheel(rows: number): void {
    const term = this.attachment?.term();
    if (!term || rows === 0) return;
    this.claim();
    if (!term.alternate) {
      term.scroll(rows);
      this.paint();
      return;
    }
    if (!term.mode(ALTERNATE_SCROLL_MODE)) return;
    const code = rows < 0 ? "ArrowUp" : "ArrowDown";
    const arrow = this.hub.keys.encode(term, { code, key: code, action: "press", shift: false, ctrl: false, alt: false, meta: false });
    const bytes = new Uint8Array(arrow.length * Math.abs(rows));
    for (let i = 0; i < Math.abs(rows); i++) bytes.set(arrow, i * arrow.length);
    this.sendInput(bytes);
  }

  view(rows: number | "live"): void {
    const term = this.attachment?.term();
    if (!term || term.alternate) return;
    if (rows === "live") this.backToLive();
    else if (rows !== 0) {
      term.scroll(rows);
      this.paint();
    }
  }

  private backToLive(): void {
    const term = this.attachment?.term();
    if (!term) return;
    const { total, offset, len } = term.scrollbar;
    if (offset + len >= total) return;
    term.scroll("bottom");
    this.paint();
  }

  text(text: string): void {
    if (!this.attachment?.term()) return;
    this.sendInput(new TextEncoder().encode(text));
  }

  paste(text: string): void {
    const term = this.attachment?.term();
    if (!term) return;
    this.sendPaste(this.hub.engine.encodePaste(text, term.mode(2004)));
  }

  setViewport(size: Size, profile: Profile, active: boolean): void {
    this.viewport = size;
    this.profile = profile;
    this.active = active;
    const line = lineOf(this.snapshot);
    if (!line || line.attachment.ended()) return;
    this.applyProfile(line.attachment.term());
    this.paint();
    if (active) this.report(line);
  }

  private report({ conn, channel }: Line): void {
    this.sizeTakenByOtherClient = false;
    conn.sendJson(FrameType.Viewport, channel, { ...this.viewport, profile: this.profile });
  }

  private claim(): void {
    const line = lineOf(this.snapshot);
    if (this.sizeTakenByOtherClient && this.active && line && !line.attachment.ended()) this.report(line);
  }

  focus(focused: boolean): void {
    this.focused = focused;
    this.recheck();
  }

  recheck(): void {
    const session = this.session === undefined ? undefined : this.hub.session(this.session);
    if (session) this.changed(session);
  }

  changed(session: Session): void {
    if (!this.ackOnFocus || !this.focused || this.acking || session.id !== this.session) return;
    if (!this.attachment?.term() || !ACKABLE_ATTENTION_STATES.has(session.attention?.state ?? "")) return;
    const conn = this.hub.live();
    if (!conn) return;
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

function describe(exit: Exit): string {
  return "code" in exit ? `code ${exit.code}` : `signal ${exit.signal}`;
}

/** Serves a page's `connect` on `port`: the worker module the page starts
 * calls `runWorker(self)`. */
export function runWorker(port: WorkerPort): void {
  const hub = new Hub((message, options) => port.postMessage(message, options));
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
      case "visibility":
        mount.visibility(message.visible);
        break;
      case "painted":
        mount.painted();
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
