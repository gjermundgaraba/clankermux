/**
 * The engine side of the page: the connection, the attachment and the
 * render state live here; the page draws frames and reports input.
 */
import { type Action, Attachment, ClientError, Connection, Osc52Scanner, type Transport, hello } from "@clankermux/client";
import { ENGINE, Engine, KeyEncoder, RenderState, type Terminal } from "@clankermux/engine";
import {
  type ErrorMessage,
  type Event,
  FrameType,
  type Page,
  type Profile,
  type Session,
  type Size,
} from "@clankermux/proto";

import type { ToPage, ToWorker } from "./protocol.ts";

interface Port {
  postMessage(message: ToPage): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
}

const port = self as unknown as Port;
const post = (message: ToPage) => port.postMessage(message);

/** Coalescing window for paints. */
const PAINT_MS = 16;
const BACKOFF_MIN_MS = 250;
const BACKOFF_MAX_MS = 5000;
/** Ending errors of an OPEN that a new OPEN would only get again. */
const PERMANENT = new Set<ErrorMessage["code"]>(["no_such_session", "engine_mismatch", "invalid_request"]);

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

class Client {
  private engine!: Engine;
  private render!: RenderState;
  private keys!: KeyEncoder;
  private url = "";
  private viewport!: Size;
  private profile!: Profile;
  private conn: Connection | undefined;
  private attachment: Attachment | undefined;
  private channel = 0;
  /** The session attached or being attached. */
  private session: string | undefined;
  private sessions = new Map<string, Session>();
  /** Input in order: a paste goes as `write` requests, and keys typed
   * meanwhile wait behind it. `attach` starts a fresh chain; input queued
   * for an attachment that is gone is dropped, but a paste under way
   * finishes (cut short, it would leave the program inside a bracketed
   * paste). */
  private writes: Promise<void> = Promise.resolve();
  private readonly clipboard = new Osc52Scanner();
  private paintTimer: ReturnType<typeof setTimeout> | undefined;
  /** Shared by reconnects and re-attaches after a failure; reset by a screen. */
  private backoff = BACKOFF_MIN_MS;
  private reopenTimer: ReturnType<typeof setTimeout> | undefined;

  private delay(): number {
    const delay = this.backoff;
    this.backoff = Math.min(this.backoff * 2, BACKOFF_MAX_MS);
    return delay;
  }

  async init(message: Extract<ToWorker, { type: "init" }>): Promise<void> {
    this.url = message.url;
    this.viewport = message.viewport;
    this.profile = message.profile;
    this.session = message.session;
    post({ type: "status", text: "loading engine" });
    const wasm = await (await fetch(message.wasm)).arrayBuffer();
    this.engine = await Engine.load(wasm);
    this.render = new RenderState(this.engine);
    this.keys = new KeyEncoder(this.engine);
    await this.connect();
  }

  private async connect(): Promise<void> {
    post({ type: "status", text: this.attachment ? "reconnecting" : "connecting" });
    let conn: Connection;
    try {
      conn = await Connection.connect(await SocketTransport.connect(this.url), hello("clankermux-web", ENGINE));
    } catch (err) {
      // A server that answers and refuses is not retried; a lost one is.
      if (err instanceof ClientError && (err.kind === "server" || err.kind === "protocol")) {
        post({ type: "status", text: err.message });
        return;
      }
      setTimeout(() => void this.connect(), this.delay());
      return;
    }
    this.conn = conn;
    conn.onEnd = (err) => {
      if (this.conn !== conn) return;
      this.conn = undefined;
      if (err.kind === "server") {
        post({ type: "status", text: err.message });
      } else {
        setTimeout(() => void this.connect(), this.delay());
      }
    };
    conn.onEvent = (event) => this.event(event);
    try {
      // Events arriving between pages are applied to the same map: a
      // later page cannot hold a session removed before it was produced.
      this.sessions.clear();
      let page = await conn.call<Page>("subscribe", {});
      for (;;) {
        for (const s of page.sessions) this.sessions.set(s.id, s);
        if (page.next === undefined) break;
        page = await conn.call<Page>("list", { after: page.next });
      }
    } catch (err) {
      post({ type: "status", text: String(err) });
      return;
    }
    this.postSessions();
    if (this.session !== undefined) this.open();
    else post({ type: "status", text: "no session" });
  }

  private event(event: Event): void {
    switch (event.event) {
      case "session_created":
      case "session_changed":
      case "session_exited":
        this.sessions.set(event.session.id, event.session);
        break;
      case "session_removed":
        this.sessions.delete(event.id);
        break;
    }
    this.postSessions();
  }

  private postSessions(): void {
    post({ type: "sessions", sessions: [...this.sessions.values()], current: this.session });
  }

  /** Opens a channel for `this.session`, resuming when the attachment can. */
  private open(): void {
    const conn = this.conn;
    if (!conn || this.session === undefined) return;
    if (!this.attachment) this.attachment = new Attachment(this.engine);
    const attachment = this.attachment;
    attachment.openOn(conn.welcome);
    const channel = ++this.channel;
    const open = {
      session: this.session,
      viewport: this.viewport,
      profile: this.profile,
      resume: attachment.resume(),
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
          post({ type: "status", text: String(err) });
          if (err instanceof ClientError && err.kind === "desync") this.reopen();
          else this.giveUp();
          return;
        }
        if (frame.header.type === FrameType.Attached && actions.length === 0) {
          post({ type: "status", text: "attached" });
        }
        for (const action of actions) this.apply(action);
      });
    } catch (err) {
      post({ type: "status", text: String(err) });
    }
  }

  /** Detaches for good: a reconnect does not try the session again. */
  private giveUp(): void {
    this.detach();
    this.session = undefined;
    this.postSessions();
  }

  /** Detaches and attaches the same session again after a backoff. */
  private reopen(): void {
    this.detach();
    this.reopenTimer ??= setTimeout(() => {
      this.reopenTimer = undefined;
      if (this.session !== undefined && !this.attachment) this.open();
    }, this.delay());
  }

  private apply(action: Action): void {
    switch (action.type) {
      case "screen": {
        this.backoff = BACKOFF_MIN_MS;
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
        post({ type: "status", text });
        break;
      }
      case "output":
        for (const write of this.clipboard.feed(action.bytes)) {
          // Only the clipboard selection; primary and secondary have no
          // browser equivalent.
          if (write.selection === "" || write.selection.includes("c")) post({ type: "clipboard", text: write.text });
        }
        this.paintTimer ??= setTimeout(() => this.paint(), PAINT_MS);
        break;
      case "resized":
        this.paint();
        break;
      case "exited":
        this.paint();
        post({ type: "status", text: `exited: ${describe(action.exit)}` });
        break;
      case "gap":
        this.clipboard.reset();
        post({ type: "status", text: "resynchronising" });
        break;
      case "closed":
        post({ type: "status", text: `closed: ${action.reason}` });
        break;
      case "error":
        post({ type: "status", text: `${action.error.code}: ${action.error.message}` });
        // An error ended the attachment: attach again after a backoff
        // (`too_slow`, a snapshot that could not be taken), unless a new
        // OPEN would fail the same way.
        if (this.attachment?.closed()) {
          if (PERMANENT.has(action.error.code)) this.giveUp();
          else this.reopen();
        }
        break;
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
    this.render = new RenderState(this.engine);
    this.paint();
  }

  private paint(): void {
    if (this.paintTimer !== undefined) {
      clearTimeout(this.paintTimer);
      this.paintTimer = undefined;
    }
    const term = this.attachment?.term();
    if (!term) return;
    post({ type: "frame", frame: this.render.update(term) });
    this.render.clean();
  }

  /** Closes the current attachment's channel and forgets it. An
   * attachment the server ended needs no CLOSE; one whose stream ended
   * (EXITED) does. */
  private detach(): void {
    if (!this.attachment) return;
    if (this.conn) {
      this.conn.release(this.channel);
      if (!this.attachment.closed()) {
        try {
          this.conn.send(FrameType.Close, this.channel, new Uint8Array());
        } catch {
          // The connection is gone; nothing to close.
        }
      }
    }
    this.attachment.free();
    this.attachment = undefined;
  }

  attach(session: string): void {
    this.detach();
    this.writes = Promise.resolve();
    this.session = session;
    this.postSessions();
    this.open();
  }

  async create(): Promise<void> {
    const conn = this.conn;
    if (!conn) return;
    try {
      const session = await conn.call<Session>("create", { cols: this.viewport.cols, rows: this.viewport.rows });
      this.attach(session.id);
    } catch (err) {
      post({ type: "status", text: String(err) });
    }
  }

  /** Input for the session, dropped unless attached on a live connection.
   * Keys go as INPUT; a paste goes as `write` requests, each answered
   * once the PTY took it, since INPUT frames are not acknowledged and a
   * paste would overflow the input queue. This tab's input is sent in
   * order, so none of it lands inside a paste. */
  private input(bytes: Uint8Array, paste: boolean): void {
    const conn = this.conn;
    const attachment = this.attachment;
    const session = this.session;
    const channel = this.channel;
    if (bytes.length === 0 || !conn || !attachment || attachment.ended() || session === undefined) return;
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
          await conn.call("write", { session, data: base64(bytes.subarray(at, at + chunk)) });
        }
      } catch (err) {
        // A connection loss is reported through onEnd; a refused chunk
        // ends the paste there.
        if (conn === this.conn) post({ type: "status", text: String(err) });
      }
    });
  }

  key(input: Extract<ToWorker, { type: "key" }>["input"]): void {
    const term = this.attachment?.term();
    if (!term) return;
    this.input(this.keys.encode(term, input), false);
  }

  paste(text: string): void {
    const term = this.attachment?.term();
    if (!term) return;
    this.input(this.engine.encodePaste(text, term.mode(2004)), true);
  }

  setViewport(size: Size, profile: Profile): void {
    this.viewport = size;
    this.profile = profile;
    const attachment = this.attachment;
    if (!this.conn || !attachment || attachment.ended()) return;
    this.applyProfile(attachment.term());
    this.paint();
    try {
      this.conn.sendJson(FrameType.Viewport, this.channel, { ...size, profile });
    } catch {
      // Reported through onEnd.
    }
  }
}

function describe(exit: { code: number } | { signal: string }): string {
  return "code" in exit ? `code ${exit.code}` : `signal ${exit.signal}`;
}

const client = new Client();
port.onmessage = (e) => {
  const message = e.data;
  switch (message.type) {
    case "init":
      void client.init(message);
      break;
    case "attach":
      client.attach(message.session);
      break;
    case "create":
      void client.create();
      break;
    case "key":
      client.key(message.input);
      break;
    case "paste":
      client.paste(message.text);
      break;
    case "viewport":
      client.setViewport(message.size, message.profile);
      break;
    case "repaint":
      client.repaint();
      break;
  }
};

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let at = 0; at < bytes.length; at += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  }
  return btoa(binary);
}
