import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterEach, beforeAll, expect, test, vi } from "vitest";

import { Engine, Terminal } from "../../src/engine/index.ts";
import { type Frame, FrameType, type Session, encodeFrame, parseFrame } from "../../src/proto/index.ts";

import { runWorker as runHead } from "../../src/client/worker.ts";
import { output, session as sessionBody, welcome } from "./bodies.ts";
import { runWorker as runTrunk, staleReopenTimers } from "./worker-366e76b.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const wasm = readFileSync(`${here}../../ghostty-vt.wasm`);
const SEEDS = Number(process.env.DIFF_SEEDS ?? 100);
const STEPS = Number(process.env.DIFF_STEPS ?? 100);
const FROM = Number(process.env.DIFF_FROM ?? 1);
let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(wasm);
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function rng(seed: number) {
  let s = seed >>> 0 || 1;
  const next = () => {
    s ^= s << 13;
    s >>>= 0;
    s ^= s >>> 17;
    s ^= s << 5;
    s >>>= 0;
    return s / 0x100000000;
  };
  return {
    int: (n: number) => Math.floor(next() * n),
    pick: <T,>(xs: readonly T[]) => xs[Math.floor(next() * xs.length)]!,
    chance: (p: number) => next() < p,
  };
}

type Status = "running" | "exited" | "restored";

type Ev =
  | { k: "mount"; m: number; session?: string; active: boolean; visible: boolean; readOnly: boolean; ackOnFocus: boolean }
  | { k: "unmount"; m: number }
  | { k: "attach"; m: number; session: string }
  | { k: "detach"; m: number }
  | { k: "key"; m: number; code: string; shift: boolean }
  | { k: "text"; m: number }
  | { k: "paste"; m: number; big: boolean }
  | { k: "select"; m: number }
  | { k: "wheel"; m: number; rows: number }
  | { k: "view"; m: number; rows: number | "live" }
  | { k: "viewport"; m: number; cols: number; fg?: string; bg?: string; active: boolean }
  | { k: "focus"; m: number; focused: boolean }
  | { k: "visibility"; m: number; visible: boolean }
  | { k: "repaint"; m: number }
  | { k: "welcome" }
  | { k: "reply"; gap: boolean }
  | { k: "replyError" }
  | { k: "openSnapshot"; status: Status; resume: boolean; pick: number }
  | { k: "openError"; code: string; pick: number }
  | { k: "output"; valid: boolean; pick: number }
  | { k: "resized"; cols: number; pick: number }
  | { k: "exited"; pick: number }
  | { k: "closed"; reason: string; pick: number }
  | { k: "gap"; pick: number }
  | { k: "chanError"; code: string; pick: number }
  | { k: "drop" }
  | { k: "fatal" }
  | { k: "down" }
  | { k: "up" }
  | { k: "restart" }
  | { k: "sessionEvent"; session: string; attention: string }
  | { k: "time"; ms: number }
  | { k: "outputPending"; pick: number }
  | { k: "progress"; status: Status };

function randomEvents(seed: number, steps: number): Ev[] {
  const r = rng(seed);
  const mounted = new Set<number>();
  const evs: Ev[] = [];
  const S = ["s", "t", "u"];
  for (let i = 0; i < steps; i++) {
    const m = r.pick([1, 1, 1, 2]);
    const roll = r.int(100);
    if (!mounted.has(m) && (roll < 30 || mounted.size === 0)) {
      mounted.add(m);
      evs.push({
        k: "mount",
        m,
        ...(r.chance(0.8) ? { session: r.pick(S) } : {}),
        active: r.chance(0.8),
        visible: r.chance(0.9),
        readOnly: r.chance(0.08),
        ackOnFocus: r.chance(0.3),
      });
      continue;
    }
    const kinds: [number, () => Ev][] = [
      [3, () => (mounted.delete(m), { k: "unmount", m })],
      [5, () => ({ k: "attach", m, session: r.pick(S) })],
      [3, () => ({ k: "detach", m })],
      [6, () => ({ k: "key", m, code: r.pick(["KeyA", "PageUp", "Enter"]), shift: r.chance(0.3) })],
      [4, () => ({ k: "text", m })],
      [4, () => ({ k: "paste", m, big: r.chance(0.2) })],
      [5, () => ({ k: "select", m })],
      [2, () => ({ k: "wheel", m, rows: r.pick([-3, 2]) })],
      [2, () => ({ k: "view", m, rows: r.pick([-2, "live"] as const) })],
      [
        4,
        () => ({
          k: "viewport",
          m,
          cols: r.pick([80, 90]),
          ...(r.chance(0.7) ? { fg: r.pick(["#112233", "#ffffff"]) } : {}),
          ...(r.chance(0.5) ? { bg: r.pick(["#000000", "#334455"]) } : {}),
          active: r.chance(0.8),
        }),
      ],
      [2, () => ({ k: "focus", m, focused: r.chance(0.6) })],
      [2, () => ({ k: "visibility", m, visible: r.chance(0.7) })],
      [1, () => ({ k: "repaint", m })],
      [8, () => ({ k: "welcome" })],
      [10, () => ({ k: "reply", gap: r.chance(0.3) })],
      [1, () => ({ k: "replyError" })],
      [
        10,
        () => ({
          k: "openSnapshot",
          status: r.pick(["running", "running", "running", "exited", "restored"] as const),
          resume: r.chance(0.7),
          pick: r.int(3),
        }),
      ],
      [3, () => ({ k: "openError", code: r.pick(["internal", "no_such_session", "engine_mismatch", "read_only", "invalid_request"]), pick: r.int(3) })],
      [6, () => ({ k: "output", valid: r.chance(0.85), pick: r.int(3) })],
      [2, () => ({ k: "resized", cols: r.pick([10, 12]), pick: r.int(3) })],
      [1, () => ({ k: "outputPending", pick: r.int(3) })],
      [2, () => ({ k: "exited", pick: r.int(3) })],
      [3, () => ({ k: "closed", reason: r.pick(["stopping", "closed", "removed", "detached"]), pick: r.int(3) })],
      [1, () => ({ k: "gap", pick: r.int(3) })],
      [2, () => ({ k: "chanError", code: r.pick(["internal", "read_only", "not_running", "no_such_session"]), pick: r.int(3) })],
      [4, () => ({ k: "drop" })],
      [1, () => ({ k: "fatal" })],
      [1, () => ({ k: "down" })],
      [2, () => ({ k: "up" })],
      [1, () => ({ k: "restart" })],
      [1, () => ({ k: "sessionEvent", session: r.pick(S), attention: r.pick(["done", "error", "working"]) })],
      [9, () => ({ k: "time", ms: r.pick([5, 20, 100, 300, 700, 1500]) })],
      [45, () => ({ k: "progress", status: r.pick(["running", "running", "running", "running", "exited", "restored"] as const) })],
    ];
    let x = r.int(kinds.reduce((a, [w]) => a + w, 0));
    for (const [w, make] of kinds) {
      if (x < w) {
        evs.push(make());
        break;
      }
      x -= w;
    }
  }
  return evs;
}

interface OpenRec {
  channel: number;
  body: { session: string; resume?: { incarnation: string; offset: number; marker: number } };
  state: "pending" | "streaming" | "ended";
  over?: boolean;
  offset: number;
  marker: number;
}

class World {
  log: string[] = [];
  sockets: FakeSocket[] = [];
  up = true;
  incarnation = 1;
  seq = 0;
  sessions = new Map<string, Session>([
    ["s", sessionBody("s")],
    ["t", sessionBody("t")],
  ]);
}
let world: World;
const FRAME_NAMES = new Map<number, string>(Object.entries(FrameType).map(([name, type]) => [type, name]));

{
  const proto = Terminal.prototype;
  const { setColor, select } = proto;
  proto.setColor = function (this: Terminal, ...a: Parameters<Terminal["setColor"]>) {
    world.log.push(`C setColor ${a.join(" ")}`);
    return setColor.apply(this, a);
  };
  proto.select = function (this: Terminal, ...a: Parameters<Terminal["select"]>) {
    world.log.push(`C select ${a[0] === undefined ? "none" : "some"}`);
    return select.apply(this, a);
  };
}

class FakeSocket {
  binaryType = "";
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: ArrayBuffer }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  idx: number;
  closed = false;
  hello = false;
  welcomed = false;
  pending: { id: number; method: string; params: Record<string, unknown> }[] = [];
  opens: OpenRec[] = [];
  canon = new Map<number, string>();

  constructor(readonly url: string) {
    this.idx = world.sockets.length;
    world.sockets.push(this);
    const up = world.up;
    queueMicrotask(() => {
      if (up) this.onopen?.();
      else {
        this.closed = true;
        this.onerror?.();
      }
    });
  }

  ch(channel: number): string {
    return this.canon.get(channel) ?? `DEAD${channel}`;
  }

  send(bytes: Uint8Array): void {
    const f: Frame = parseFrame(bytes, 1 << 22);
    const type = f.header.type;
    const ch = f.header.channel;
    const text = () => new TextDecoder().decode(f.payload);
    if (this.closed) {
      world.log.push(`S${this.idx} AFTER-CLOSE ${FRAME_NAMES.get(type) ?? type}`);
      return;
    }
    const rec = this.opens.find((o) => o.channel === ch);
    const ended = rec?.state === "ended" ? " (ended)" : "";
    let desc: string;
    switch (type) {
      case FrameType.Hello:
        this.hello = true;
        desc = "HELLO";
        break;
      case FrameType.Request: {
        const body = JSON.parse(text()) as { id: number; method: string; params: Record<string, unknown> };
        this.pending.push(body);
        const params = body.method === "send" ? { session: body.params.session, len: String(body.params.data).length } : body.params;
        desc = `REQ ${body.id} ${body.method} ${JSON.stringify(params)}`;
        break;
      }
      case FrameType.Open: {
        const body = JSON.parse(text());
        const canon = `o${this.opens.length}`;
        this.canon.set(ch, canon);
        this.opens.push({ channel: ch, body, state: "pending", offset: 0, marker: 0 });
        desc = `OPEN ${canon} ${JSON.stringify(body)}`;
        break;
      }
      case FrameType.Close:
        desc = `CLOSE ${this.ch(ch)}${rec ? ` (${rec.state})` : ""}`;
        if (rec) rec.state = "ended";
        break;
      case FrameType.Input:
        desc = `INPUT ${this.ch(ch)}${ended} ${JSON.stringify(text())}`;
        break;
      case FrameType.Viewport:
        desc = `VIEWPORT ${this.ch(ch)}${ended} ${text()}`;
        break;
      default:
        desc = `${FRAME_NAMES.get(type) ?? type} ${this.ch(ch)} ${text()}`;
    }
    world.log.push(`S${this.idx} ${desc}`);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    world.log.push(`S${this.idx} client-closed`);
    queueMicrotask(() => this.onclose?.());
  }

  serverDrop(): void {
    if (this.closed) return;
    this.closed = true;
    this.onclose?.();
  }

  push(type: FrameType, channel: number, body: unknown): void {
    this.pushRaw(encodeFrame(type, channel, new TextEncoder().encode(JSON.stringify(body))));
  }

  pushRaw(bytes: Uint8Array): void {
    if (!this.closed) this.onmessage?.({ data: bytes.slice().buffer });
  }

  reply(gap: boolean): boolean {
    const req = this.pending.shift();
    if (!req) return false;
    const sessions = [...world.sessions.values()];
    let result: unknown = {};
    if (req.method === "subscribe") {
      result =
        req.params.after === undefined
          ? { sessions, oldest: 1, latest: world.seq }
          : { oldest: 1, latest: world.seq, gap: gap || req.params.incarnation !== `inc${world.incarnation}` };
    } else if (req.method === "list") result = { sessions };
    this.push(FrameType.Reply, 0, { id: req.id, result });
    return true;
  }

  answer(rec: OpenRec, status: Status, resume: boolean, text: string): void {
    const s = sessionFor(rec.body.session, status);
    const from = rec.body.resume;
    if (resume && from?.incarnation === `inc${world.incarnation}`) {
      this.push(FrameType.Attached, rec.channel, { mode: "resume", offset: from.offset, marker: from.marker, cols: 10, rows: 3, session: s });
      rec.state = "streaming";
      rec.offset = from.offset;
      rec.marker = from.marker;
    } else bootstrap(this, rec, s, text);
  }
}

function current(): FakeSocket | undefined {
  const s = world.sockets.at(-1);
  return s && !s.closed ? s : undefined;
}

function sessionFor(id: string, status: Status): Session {
  return sessionBody(id, { status, ...(status === "exited" && { exit: { code: 3 } }) });
}

function bootstrap(sock: FakeSocket, rec: OpenRec, s: Session, text: string): void {
  rec.over = s.status !== "running";
  const source = engine.terminalNew(10, 3);
  source.write(new TextEncoder().encode(text));
  const bytes = source.snapshot();
  source.free();
  sock.push(FrameType.Attached, rec.channel, { mode: "snapshot", offset: 0, marker: 0, cols: 10, rows: 3, session: s, snapshot_bytes: bytes.length });
  sock.pushRaw(encodeFrame(FrameType.Snapshot, rec.channel, bytes));
  sock.pushRaw(encodeFrame(FrameType.SnapshotReady, rec.channel, new Uint8Array()));
  sock.pushRaw(encodeFrame(FrameType.SnapshotEnd, rec.channel, new Uint8Array()));
  rec.state = "streaming";
  rec.offset = 0;
  rec.marker = 0;
}

async function flush(): Promise<void> {
  for (let i = 0; i < 30; i++) await new Promise<void>((r) => setImmediate(r));
}

function digest(value: unknown): string {
  let h = 2166136261;
  const s = JSON.stringify(value, (_k, v) => (ArrayBuffer.isView(v) ? Array.from(v as unknown as ArrayLike<number>) : v));
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619) >>> 0;
  return h.toString(16);
}

function describePost(message: { type: string; [k: string]: unknown }): string {
  switch (message.type) {
    case "frame":
      return `P frame m${message.mount} ${digest(message.frame)}`;
    case "sessions":
      return `P sessions ${(message.sessions as Session[]).map((s) => `${s.id}:${s.status}`).join(",")}`;
    default: {
      const { type, ...rest } = message;
      return `P ${type} ${JSON.stringify(rest)}`;
    }
  }
}

async function runOne(run: typeof runHead, evs: Ev[]): Promise<string[]> {
  world = new World();
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"] });
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("fetch", async () => ({ arrayBuffer: async () => wasm }));
  const port = {
    postMessage: (message: { type: string; mount?: number }, options?: StructuredSerializeOptions) => {
      world.log.push(describePost(structuredClone(message, options)));
      if (message.type === "frame") port.onmessage?.({ data: { type: "painted", mount: message.mount } } as MessageEvent);
    },
    onmessage: null as ((e: MessageEvent) => void) | null,
  };
  run(port as never);
  const send = (message: unknown) => port.onmessage!({ data: message } as MessageEvent);
  send({ type: "init", wasm: "ghostty-vt.wasm", url: "wss://x/", client: undefined });
  for (let i = 0; i < 200 && !world.log.includes("P ready {}"); i++) await flush();
  expect(world.log).toContain("P ready {}");
  await flush();
  let textN = 0;
  for (const [i, ev] of evs.entries()) {
    world.log.push(`E${i} ${JSON.stringify(ev)}`);
    const sock = current();
    const pickOpen = (pick: number, states: OpenRec["state"][]) => {
      const list = sock?.opens.filter((o) => states.includes(o.state)) ?? [];
      return list.length ? list[list.length - 1 - (pick % list.length)] : undefined;
    };
    switch (ev.k) {
      case "mount": {
        const { k: _, m, ...init } = ev;
        send({ type: "mount", mount: m, viewport: { cols: 80, rows: 24 }, profile: {}, ...init, session: ev.session });
        break;
      }
      case "unmount":
      case "detach":
      case "repaint":
        send({ type: ev.k, mount: ev.m });
        break;
      case "attach":
        send({ type: "attach", mount: ev.m, session: ev.session });
        break;
      case "key":
        send({
          type: "key",
          mount: ev.m,
          input: { code: ev.code, key: ev.code === "KeyA" ? "a" : ev.code, action: "press", shift: ev.shift, ctrl: false, alt: false, meta: false },
        });
        break;
      case "text":
        send({ type: "text", mount: ev.m, text: `t${textN++}` });
        break;
      case "paste":
        send({ type: "paste", mount: ev.m, text: ev.big ? "x".repeat(100_000) : `p${textN++}` });
        break;
      case "select":
        send({ type: "select", mount: ev.m, action: "press", at: { x: 0, y: 0 }, time: i * 1000 });
        send({ type: "select", mount: ev.m, action: "drag", at: { x: 4, y: 0 }, time: i * 1000 + 1 });
        send({ type: "select", mount: ev.m, action: "release", at: { x: 4, y: 0 }, time: i * 1000 + 2, copy: i });
        break;
      case "wheel":
      case "view":
        send({ type: ev.k, mount: ev.m, rows: ev.rows });
        break;
      case "viewport":
        send({
          type: "viewport",
          mount: ev.m,
          size: { cols: ev.cols, rows: 24 },
          profile: { ...(ev.fg && { fg: ev.fg }), ...(ev.bg && { bg: ev.bg }) },
          active: ev.active,
        });
        break;
      case "focus":
        send({ type: "focus", mount: ev.m, focused: ev.focused });
        break;
      case "visibility":
        send({ type: "visibility", mount: ev.m, visible: ev.visible });
        break;
      case "welcome":
        if (sock?.hello && !sock.welcomed) {
          sock.welcomed = true;
          sock.push(FrameType.Welcome, 0, welcome(`inc${world.incarnation}`));
        }
        break;
      case "reply":
        sock?.reply(ev.gap);
        break;
      case "replyError": {
        const at = sock?.pending.findIndex((p) => p.method === "send") ?? -1;
        if (!sock || at < 0) break;
        const [req] = sock.pending.splice(at, 1);
        sock.push(FrameType.Error, 0, { id: req!.id, code: "no_such_session", message: "gone" });
        break;
      }
      case "openSnapshot": {
        const rec = pickOpen(ev.pick, ["pending"]);
        if (sock && rec) sock.answer(rec, ev.status, ev.resume, `hello ${rec.body.session} ${i}`);
        break;
      }
      case "openError": {
        const rec = pickOpen(ev.pick, ["pending"]);
        if (!sock || !rec) break;
        sock.push(FrameType.Error, rec.channel, { code: ev.code, message: `${ev.code} ${rec.body.session}` });
        if (!["read_only", "not_running", "input_overflow"].includes(ev.code)) rec.state = "ended";
        break;
      }
      case "output": {
        const rec = pickOpen(ev.pick, ["streaming"]);
        if (!sock || !rec) break;
        const body = `o${i}\r\n`;
        sock.pushRaw(encodeFrame(FrameType.Output, rec.channel, output(ev.valid ? rec.offset : rec.offset + 7, body)));
        if (ev.valid) rec.offset += body.length;
        else rec.state = "ended";
        break;
      }
      case "outputPending": {
        const rec = pickOpen(ev.pick, ["pending"]);
        if (!sock || !rec) break;
        sock.pushRaw(encodeFrame(FrameType.Output, rec.channel, output(0, "early")));
        rec.state = "ended";
        break;
      }
      case "resized": {
        const rec = pickOpen(ev.pick, ["streaming"]);
        if (!sock || !rec) break;
        rec.marker++;
        sock.push(FrameType.Resized, rec.channel, { seq: rec.marker, offset: rec.offset, cols: ev.cols, rows: 3 });
        break;
      }
      case "exited": {
        const rec = pickOpen(ev.pick, ["streaming"]);
        if (!sock || !rec || rec.over) break;
        rec.marker++;
        sock.push(FrameType.Exited, rec.channel, { seq: rec.marker, offset: rec.offset, exit: { code: 7 } });
        rec.state = "ended";
        break;
      }
      case "closed": {
        const rec = pickOpen(ev.pick, ["streaming", "pending"]);
        if (!sock || !rec) break;
        sock.push(FrameType.Closed, rec.channel, { reason: ev.reason, ...(ev.reason === "detached" && { message: "taken" }) });
        rec.state = "ended";
        break;
      }
      case "gap": {
        const rec = pickOpen(ev.pick, ["streaming"]);
        if (!sock || !rec) break;
        sock.push(FrameType.Gap, rec.channel, { from: rec.offset, to: rec.offset + 10 });
        bootstrap(sock, rec, sessionFor(rec.body.session, "running"), `again ${i}`);
        break;
      }
      case "chanError": {
        const rec = pickOpen(ev.pick, ["streaming"]);
        if (!sock || !rec) break;
        sock.push(FrameType.Error, rec.channel, { code: ev.code, message: `${ev.code} later` });
        if (!["read_only", "not_running", "input_overflow"].includes(ev.code)) rec.state = "ended";
        break;
      }
      case "drop":
        sock?.serverDrop();
        break;
      case "fatal":
        sock?.push(FrameType.Error, 0, { code: "internal", message: "server fatal" });
        break;
      case "down":
      case "up":
        world.up = ev.k === "up";
        break;
      case "restart":
        world.incarnation++;
        sock?.serverDrop();
        break;
      case "sessionEvent": {
        if (!sock?.welcomed) break;
        const s = { ...(world.sessions.get(ev.session) ?? sessionBody(ev.session)), attention: { state: ev.attention } } as Session;
        world.sessions.set(ev.session, s);
        world.seq++;
        sock.push(FrameType.Event, 0, { seq: world.seq, event: "session_changed", session: s });
        break;
      }
      case "time":
        await vi.advanceTimersByTimeAsync(ev.ms);
        break;
      case "progress": {
        if (!sock) {
          await vi.advanceTimersByTimeAsync(300);
          break;
        }
        if (sock.hello && !sock.welcomed) {
          sock.welcomed = true;
          sock.push(FrameType.Welcome, 0, welcome(`inc${world.incarnation}`));
          break;
        }
        if (sock.reply(false)) break;
        const rec = sock.opens.find((o) => o.state === "pending");
        if (rec) sock.answer(rec, ev.status, true, `hello ${rec.body.session} ${i}`);
        else await vi.advanceTimersByTimeAsync(100);
        break;
      }
    }
    await flush();
  }
  for (const m of [1, 2]) send({ type: "unmount", mount: m });
  vi.clearAllTimers();
  vi.useRealTimers();
  return world.log;
}

const noSession = (m: string) => `P status {"mount":${m},"text":"no session"}`;
const connectedToNone = (m: string) => `P link {"mount":${m},"link":{"state":"connected"}}`;

const FRESH_REOPEN_TIMER = "a reopen after a re-attach or a reconnect during a backoff waits a fresh backoff, not the stale timer";

const ALLOWED: Record<string, (trunk: string[]) => string[]> = {
  "no INPUT or VIEWPORT on a channel the connection never opened": (trunk) =>
    trunk.filter((l) => !/^S\d+ (INPUT|VIEWPORT) DEAD\d+/.test(l)),
  "no stray CLOSE for a channel the connection never opened": (trunk) => trunk.filter((l) => !/^S\d+ CLOSE DEAD\d+/.test(l)),
  "one no session status and one connected link for a detach during a handshake": (trunk) => {
    const kept: string[] = [];
    let live = false;
    let detached: string | undefined;
    for (let i = 0; i < trunk.length; i++) {
      const l = trunk[i]!;
      if (l.startsWith("P connection ")) live = l.includes('"state":"connected"');
      if (l.startsWith("E")) detached = live ? undefined : /"k":"detach","m":(\d+)/.exec(l)?.[1];
      if (detached && l === noSession(detached) && trunk[i + 1] === connectedToNone(detached)) {
        i++;
        continue;
      }
      kept.push(l);
    }
    return kept;
  },
};

function compare(head: string[], trunk: string[]): string | undefined {
  const streams: [string, (l: string) => boolean][] = [
    ["page", (l) => !l.startsWith("S")],
    ["server", (l) => !/^[PC] /.test(l)],
  ];
  for (const [name, keep] of streams) {
    const h = head.filter(keep);
    const t = trunk.filter(keep);
    const at = h.findIndex((l, i) => l !== t[i]);
    const first = at >= 0 ? at : h.length < t.length ? h.length : -1;
    if (first < 0) continue;
    const event = h.slice(0, first).reverse().find((l) => l.startsWith("E")) ?? "";
    return `${name} @${first} after ${event}\n  head : ${h[first]}\n  trunk: ${t[first]}`;
  }
  return undefined;
}

interface Tally {
  raw: number;
  allowed: Map<string, number>;
  diffs: string[];
}

function allowedOnce(tally: Tally, name: string): void {
  tally.allowed.set(name, (tally.allowed.get(name) ?? 0) + 1);
}

async function differential(label: string, evs: Ev[], tally: Tally): Promise<void> {
  const head = await runOne(runHead, evs);
  const stale = staleReopenTimers.cleared;
  let trunk = await runOne(runTrunk, evs);
  if (staleReopenTimers.cleared !== stale) allowedOnce(tally, FRESH_REOPEN_TIMER);
  if (compare(head, trunk) === undefined) tally.raw++;
  for (const [name, allow] of Object.entries(ALLOWED)) {
    const before = trunk.length;
    trunk = allow(trunk);
    if (trunk.length !== before) allowedOnce(tally, name);
  }
  const diff = compare(head, trunk);
  if (diff) tally.diffs.push(`${label} ${diff}`);
}

function tally(): Tally {
  return { raw: 0, allowed: new Map(), diffs: [] };
}

const P = (status: Status = "running"): Ev => ({ k: "progress", status });
const T = (ms: number): Ev => ({ k: "time", ms });
const M = (session: string): Ev => ({ k: "mount", m: 1, session, active: true, visible: true, readOnly: false, ackOnFocus: false });
const SEL: Ev = { k: "select", m: 1 };
const VP: Ev = { k: "viewport", m: 1, cols: 80, fg: "#112233", bg: "#334455", active: true };
const KEY: Ev = { k: "key", m: 1, code: "KeyA", shift: false };
const TXT: Ev = { k: "text", m: 1 };
const opened: Ev[] = [M("s"), P(), P()];
const attached: Ev[] = [...opened, P()];
const sizeTakenThenReconnecting: Ev[] = [...attached, { k: "resized", cols: 12, pick: 0 }, { k: "drop" }, T(300), P()];

const SCENARIOS: Record<string, Ev[]> = {
  "a selection, then CLOSED removed": [...attached, SEL, { k: "closed", reason: "removed", pick: 0 }],
  "a selection, then a desync": [...attached, SEL, { k: "output", valid: false, pick: 0 }],
  "a selection, then a retryable channel error": [...attached, SEL, { k: "chanError", code: "internal", pick: 0 }],
  "the size taken, a colour change in the handshake, a key after the resume": [...sizeTakenThenReconnecting, VP, P(), P(), KEY],
  "the size taken, a key in the handshake, a key after the resume": [...sizeTakenThenReconnecting, KEY, P(), P(), KEY],
  "the size taken, text in the handshake, text after the resume": [...sizeTakenThenReconnecting, TXT, P(), P(), TXT],
  "the size taken, a selection in the handshake, a key after the resume": [...sizeTakenThenReconnecting, SEL, P(), P(), KEY],
  "a re-attach during a backoff, then a retryable error": [
    ...opened,
    { k: "openError", code: "internal", pick: 0 },
    { k: "attach", m: 1, session: "t" },
    T(100),
    { k: "openError", code: "internal", pick: 0 },
    ...Array.from({ length: 10 }, () => T(100)),
  ],
  "a reconnect during a backoff, then a retryable error": [
    ...opened,
    { k: "openError", code: "internal", pick: 0 },
    T(250),
    { k: "openError", code: "internal", pick: 0 },
    { k: "drop" },
    T(250),
    P(),
    P(),
    { k: "openError", code: "internal", pick: 0 },
    ...Array.from({ length: 14 }, () => T(100)),
  ],
  "a detach during the first handshake": [M("s"), P(), { k: "detach", m: 1 }, P()],
  "an attach to another session during a handshake": [...attached, { k: "drop" }, T(300), P(), { k: "attach", m: 1, session: "t" }, P(), P()],
  "an unmount during a handshake": [...attached, { k: "drop" }, T(300), P(), { k: "unmount", m: 1 }, P()],
  "a detach during a handshake that never completes": [...attached, { k: "drop" }, T(300), P(), { k: "detach", m: 1 }, { k: "drop" }],
  "an exit, then a colour change and a key": [...attached, SEL, { k: "exited", pick: 0 }, VP, KEY, T(50)],
  "a server stopping, then a handshake, a colour change and a key": [
    ...attached,
    SEL,
    { k: "closed", reason: "stopping", pick: 0 },
    { k: "drop" },
    T(300),
    P(),
    VP,
    KEY,
    T(50),
  ],
  "a detach during the handshake, then a colour change and a key": [...attached, SEL, { k: "drop" }, T(300), P(), { k: "detach", m: 1 }, VP, KEY, T(50), P(), VP, KEY],
  "a selection, a colour change and a key during the handshake": [...attached, SEL, { k: "drop" }, T(300), P(), VP, SEL, KEY, T(50)],
};

test.each(Object.entries(SCENARIOS))("%s: head posts and sends what 366e76b did, but for the allowed differences", async (name, evs) => {
  const t = tally();
  await differential(name, evs, t);
  expect(t.diffs).toEqual([]);
}, 60_000);

test(`${SEEDS} random runs of ${STEPS} events: head posts and sends what 366e76b did, but for the allowed differences`, async () => {
  const t = tally();
  for (let seed = FROM; seed < FROM + SEEDS; seed++) await differential(`seed ${seed}`, randomEvents(seed, STEPS), t);
  console.log(
    [`${SEEDS} seeds from ${FROM}, ${STEPS} events: ${t.raw} identical raw`, ...[...t.allowed].map(([name, n]) => `  ${n} seeds: ${name}`), `  ${t.diffs.length} seeds differ`].join("\n"),
  );
  expect(t.diffs).toEqual([]);
}, 3_600_000);
