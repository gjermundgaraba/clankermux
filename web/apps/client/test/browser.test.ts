/**
 * The web package in Chrome against a real server: terminals mounted from
 * one client share its connection, survive a lost connection by resuming,
 * and come back from a server restart; `wait` runs through the client and
 * an abort cancels it; a terminal that opted in marks its session's
 * records seen when it gets the focus. The page reaches the server's TLS
 * WebSocket carrier through a TCP proxy the suite counts connections on
 * and cuts. Chrome is the installed one (`CHROME` names another).
 */
import { type ChildProcess, execFile, spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { type Server, createServer } from "node:http";
import { type Socket, createServer as createTcpServer, connect as tcpConnect } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import * as esbuild from "esbuild";
import { type Browser, type BrowserContext, type Page, chromium } from "playwright-core";
import { afterAll, afterEach, beforeAll, expect, test, vi } from "vitest";

import type { Session } from "@gjermundgaraba/clankermux";


const run = promisify(execFile);
const here = fileURLToPath(new URL(".", import.meta.url));
const root = `${here}../../../../`;
const binary = `${root}target/debug/clankermux`;
// `just check` builds the binary before the web suites run; a missing one
// is a failure, not a reason to skip.
if (!existsSync(binary)) throw new Error(`${binary} is missing: run cargo build first`);
const tls = `${root}web/packages/clankermux/test/client/fixtures/tls/`;

vi.setConfig({ testTimeout: 60_000, hookTimeout: 60_000 });

/** A TCP proxy to the server's carrier: it counts the connections it
 * took, and while `down` it cuts the ones it has and refuses new ones. */
class Proxy {
  private readonly server = createTcpServer((socket) => this.take(socket));
  private readonly open = new Set<Socket>();
  accepted = 0;
  down = false;
  target = 0;

  async listen(): Promise<number> {
    await new Promise<void>((resolve) => this.server.listen(0, "127.0.0.1", resolve));
    return (this.server.address() as { port: number }).port;
  }

  private take(socket: Socket): void {
    if (this.down) return void socket.destroy();
    this.accepted++;
    const upstream = tcpConnect(this.target, "127.0.0.1");
    for (const [from, to] of [
      [socket, upstream],
      [upstream, socket],
    ] as const) {
      from.pipe(to);
      from.on("error", () => to.destroy());
      from.on("close", () => to.destroy());
      this.open.add(from);
      from.on("close", () => this.open.delete(from));
    }
  }

  cut(): void {
    this.down = true;
    for (const socket of this.open) socket.destroy();
  }

  close(): Promise<void> {
    this.cut();
    return new Promise((resolve) => this.server.close(() => resolve()));
  }
}

let dir: string;
let socketPath: string;
let server: ChildProcess | undefined;
let page_: Server;
let origin: string;
const proxy = new Proxy();
let proxyUrl: string;
let browser: Browser;

/** Starts the server on the state in `dir`; resolves with its carrier's port. */
async function serve(): Promise<void> {
  server = spawn(
    binary,
    ["--socket", socketPath, "serve", "--ws", "127.0.0.1:0", "--origin", origin, "--tls-cert", `${tls}cert.pem`, "--tls-key", `${tls}key.pem`],
    { stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, XDG_STATE_HOME: join(dir, "state") } },
  );
  const started = server;
  proxy.target = await new Promise<number>((resolve, reject) => {
    let seen = "";
    started.stderr!.on("data", (chunk: Buffer) => {
      seen += chunk;
      const m = /listening on wss:\/\/[^:]+:(\d+)/.exec(seen);
      if (m) resolve(Number(m[1]));
    });
    started.once("exit", (code) => reject(new Error(`server exited: ${code}`)));
  });
  await eventually("the server's socket", () => cli("list").then(() => true).catch(() => undefined));
}

/** Stops the server as a machine's shutdown does: it parks its sessions. */
async function stop(): Promise<void> {
  const running = server;
  if (!running || running.exitCode !== null) return;
  await new Promise((resolve) => (running.once("exit", resolve), running.kill()));
}

const cli = async (...args: string[]) => (await run(binary, ["--socket", socketPath, ...args])).stdout;
const list = async (): Promise<Session[]> => JSON.parse(await cli("--json", "list"));
const create = async (...argv: string[]): Promise<string> => JSON.parse(await cli("--json", "create", "--", ...argv)).id;
const listed = async (id: string) => (await list()).find((s) => s.id === id);

async function eventually<A>(what: string, probe: () => Promise<A | undefined>, timeoutMs = 15_000): Promise<A> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = await probe();
    if (found !== undefined) return found;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), "cmx-browser-"));
  socketPath = join(dir, "sock");
  const dist = join(dir, "page");
  await esbuild.build({
    entryPoints: { main: `${here}page/main.ts`, worker: `${here}../src/worker.ts` },
    bundle: true,
    format: "esm",
    target: "es2022",
    outdir: dist,
    logLevel: "silent",
  });
  copyFileSync(`${here}page/index.html`, join(dist, "index.html"));
  copyFileSync(`${root}web/packages/clankermux/ghostty-vt.wasm`, join(dist, "ghostty-vt.wasm"));
  const types: Record<string, string> = { html: "text/html", js: "text/javascript", wasm: "application/wasm" };
  page_ = createServer((request, response) => {
    const name = (request.url ?? "/").split("?")[0]!.slice(1) || "index.html";
    const path = join(dist, name);
    if (name.includes("..") || !existsSync(path)) return void response.writeHead(404).end();
    response.writeHead(200, { "content-type": types[name.split(".").at(-1)!] ?? "application/octet-stream" });
    response.end(readFileSync(path));
  });
  await new Promise<void>((resolve) => page_.listen(0, "127.0.0.1", resolve));
  origin = `http://127.0.0.1:${(page_.address() as { port: number }).port}`;
  proxyUrl = `wss://127.0.0.1:${await proxy.listen()}/`;
  await serve();
  browser = await chromium.launch({
    ...(process.env["CHROME"] ? { executablePath: process.env["CHROME"] } : { channel: "chrome" }),
    // The fixture certificate is self-signed.
    args: ["--ignore-certificate-errors"],
  });
});

// A failed test leaves neither its page reconnecting nor the proxy down.
const contexts: { close(): Promise<void> }[] = [];
afterEach(async () => {
  for (const context of contexts.splice(0)) await context.close().catch(() => {});
  proxy.down = false;
});

afterAll(async () => {
  await browser?.close();
  await stop();
  await proxy.close();
  page_?.close();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

/** A fresh page, connected through the proxy; in a fresh context unless
 * given one. */
async function opened(given?: BrowserContext): Promise<Page> {
  const context = given ?? (await browser.newContext({ ignoreHTTPSErrors: true }));
  contexts.push(context);
  const page = await context.newPage();
  page.on("pageerror", (err) => console.error(err));
  page.on("console", (message) => message.type() === "error" && console.error(message.text()));
  await page.goto(origin);
  await page.waitForFunction(() => "harness" in window);
  await page.evaluate((url) => harness().connect(url), proxyUrl);
  return page;
}

const linkOf = (page: Page, name: string) => page.evaluate((n) => harness().link(n), name);

const connectedTo = (page: Page, name: string, status = "running") =>
  eventually(`${name} connected`, async () => {
    const link = await linkOf(page, name);
    return link?.state === "connected" && link.session?.status === status ? link : undefined;
  });

/** Types `line` and Enter into the terminal `name`. */
async function typed(page: Page, name: string, line: string): Promise<void> {
  await page.click(`#${name}`);
  await page.keyboard.type(`${line}\n`);
}

const screenOf = async (id: string) => await cli("read", id);

test("two terminals mounted from one client share its one connection", async () => {
  const left = await create("/bin/cat");
  const right = await create("/bin/cat");
  const before = proxy.accepted;
  const page = await opened();
  await page.evaluate(([l, r]) => (harness().mount("left", l!), harness().mount("right", r!)), [left, right]);
  await connectedTo(page, "left");
  await connectedTo(page, "right");
  expect(proxy.accepted - before).toBe(1);
  for (const id of [left, right]) {
    const attachments = (await listed(id))!.attachments;
    expect(attachments.map((a) => a.client.name)).toEqual(["clankermux-web"]);
  }

  // Each terminal's keys reach its own session.
  await typed(page, "left", "to-the-left");
  await typed(page, "right", "to-the-right");
  await eventually("the left line", async () => ((await screenOf(left)).includes("to-the-left\nto-the-left") ? true : undefined));
  await eventually("the right line", async () => ((await screenOf(right)).includes("to-the-right\nto-the-right") ? true : undefined));
  expect(await screenOf(left)).not.toContain("to-the-right");
  expect(await screenOf(right)).not.toContain("to-the-left");
  expect(proxy.accepted - before).toBe(1);
  await page.context().close();
});

test("a lost connection: each terminal resumes, and the events missed meanwhile are replayed", async () => {
  const left = await create("/bin/cat");
  const right = await create("/bin/cat");
  const page = await opened();
  await page.evaluate(([l, r]) => (harness().mount("left", l!), harness().mount("right", r!)), [left, right]);
  await connectedTo(page, "left");
  await connectedTo(page, "right");
  const before = proxy.accepted;

  proxy.cut();
  await eventually("the link lost", async () => ((await linkOf(page, "left"))?.state === "connecting" ? true : undefined));
  // Output and a bell while the page is away.
  await cli("send", left, "\x07rung-while-away", "--enter");
  // Echoed by the terminal, then printed by `cat`, with the bell.
  await eventually("the line printed", async () => ((await screenOf(left)).split("rung-while-away").length > 2 ? true : undefined));
  proxy.down = false;

  for (const name of ["left", "right"]) {
    await connectedTo(page, name);
    await eventually(`${name} resumed`, async () => ((await page.evaluate((n) => harness().statuses(n).at(-1), name)) === "resumed" ? true : undefined));
  }
  expect(proxy.accepted - before).toBe(1);
  await eventually("the bell replayed", async () => {
    const events = await page.evaluate(() => harness().events());
    return events.some((e) => e.event === "bell" && e.id === left) ? events : undefined;
  });
  expect((await page.evaluate(() => harness().events())).some((e) => e.event === "gap")).toBe(false);

  // And input after the resume goes through.
  await typed(page, "right", "after-the-resume");
  await eventually("the line typed after", async () =>
    (await screenOf(right)).includes("after-the-resume\nafter-the-resume") ? true : undefined,
  );
  await page.context().close();
});

test("a server restart: the terminals come back to their restored sessions, and the gap is told", async () => {
  const left = await create("/bin/cat");
  const right = await create("/bin/cat");
  const page = await opened();
  await page.evaluate(([l, r]) => (harness().mount("left", l!), harness().mount("right", r!)), [left, right]);
  await connectedTo(page, "left");
  await connectedTo(page, "right");

  await stop();
  await serve();
  expect((await listed(left))?.status).toBe("restored");
  await connectedTo(page, "left", "restored");
  await connectedTo(page, "right", "restored");
  await eventually("the gap", async () => ((await page.evaluate(() => harness().events())).some((e) => e.event === "gap") ? true : undefined));
  await page.context().close();
});

test("under the kitty keyboard protocol, shifted symbols reach the program as typed", async () => {
  // Disambiguation, as neovim asks for it; the program prints the bytes
  // of four keys in hex, once the terminal is raw.
  const session = await create("/bin/sh", "-c", "printf '\\033[>1u'; stty raw -echo; printf ready; dd bs=1 count=4 2>/dev/null | od -An -tx1; exec cat");
  const page = await opened();
  await page.evaluate((s) => harness().mount("only", s), session);
  await connectedTo(page, "only");
  await eventually("the terminal raw", async () => ((await screenOf(session)).includes("ready") ? true : undefined));
  await page.click("#only");
  // Shift held, as a keyboard sends it: whatever the machine's layout has
  // on these keys unshifted, the text is what Shift made.
  await page.keyboard.down("Shift");
  for (const key of ["Semicolon", "Quote", "Digit1"]) await page.keyboard.press(key);
  await page.keyboard.up("Shift");
  await page.keyboard.press("Enter");
  await eventually("the bytes printed", async () =>
    (await screenOf(session)).replace(/\s+/g, " ").includes("3a 22 21 0d") ? true : undefined,
  );
  await page.context().close();
});

test("a key the input method processes puts the input element at the cursor, with key code 229 or without", async () => {
  const session = await create("/bin/sh", "-c", "printf abcdef; exec cat");
  const page = await opened();
  await page.evaluate((s) => harness().mount("only", s), session);
  await connectedTo(page, "only");
  // `Process` as a browser may report it: key code 0, not composing. The
  // input method's window opens at the input element, which follows the
  // cursor after `abcdef` on the first row.
  await eventually("the input element at the cursor", () =>
    page.evaluate(() => {
      const canvas = document.querySelector<HTMLCanvasElement>("#only")!;
      const input = canvas.nextElementSibling as HTMLTextAreaElement;
      input.dispatchEvent(new KeyboardEvent("keydown", { code: "KeyA", key: "Process" }));
      const cell = canvas.getBoundingClientRect();
      const at = input.getBoundingClientRect();
      return input.style.left !== "" && at.top === cell.top && at.left > cell.left && at.left < cell.right ? true : undefined;
    }),
  );
  await page.context().close();
});

test("a selection's copy is told to the page once the clipboard has it", async () => {
  const session = await create("/bin/sh", "-c", "printf 'copyme\\n'; exec cat");
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin });
  const page = await opened(context);
  await page.evaluate((s) => harness().mount("only", s), session);
  await connectedTo(page, "only");
  await eventually("the word drawn", async () => ((await screenOf(session)).includes("copyme") ? true : undefined));
  // A double click selects the word; the first click selected nothing,
  // which is no copy.
  const box = (await page.locator("#only").boundingBox())!;
  await page.mouse.dblclick(box.x + 10, box.y + 8);
  const copies = await eventually("the copy told", async () => {
    const told = await page.evaluate(() => harness().copies("only"));
    return told.length > 0 ? told : undefined;
  });
  expect(copies).toEqual([{ ok: true, source: "release", chars: 6 }]);
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe("copyme");
  await new Promise((resolve) => setTimeout(resolve, 300));
  expect(await page.evaluate(() => harness().copies("only"))).toHaveLength(1);
  await context.close();

  // With the permission denied Chrome refuses the write, and the page is
  // told.
  const denied = await browser.newContext({ ignoreHTTPSErrors: true });
  const target = await (await denied.newCDPSession(await denied.newPage())).send("Target.getTargetInfo");
  await (await browser.newBrowserCDPSession()).send("Browser.setPermission", {
    permission: { name: "clipboard-write" },
    setting: "denied",
    origin,
    browserContextId: target.targetInfo.browserContextId!,
  });
  const refused = await opened(denied);
  await refused.evaluate((s) => harness().mount("only", s), session);
  await connectedTo(refused, "only");
  const at = (await refused.locator("#only").boundingBox())!;
  await refused.mouse.dblclick(at.x + 10, at.y + 8);
  const failed = await eventually("the refusal told", async () => {
    const told = await refused.evaluate(() => harness().copies("only"));
    return told.length > 0 ? told : undefined;
  });
  expect(failed).toEqual([{ ok: false, source: "release", reason: "refused", chars: 6, message: expect.stringMatching(/^NotAllowedError/) }]);
  await refused.context().close();
});

test("wait through the client: a match resolves it, an abort cancels it on the server", async () => {
  const session = await create("/bin/cat");
  const page = await opened();
  await page.evaluate((s) => harness().mount("only", s), session);
  await connectedTo(page, "only");

  await page.evaluate((s) => harness().startWait(s, { output: { text: "waited-for" } }), session);
  await typed(page, "only", "waited-for");
  const waited = await page.evaluate(() => harness().waited());
  expect(waited.reason).toBe("matched");
  expect(waited.match?.text).toBe("waited-for");

  const aborted = await page.evaluate((s) => harness().wait(s, { output: { text: "never-printed" } }, 200), session);
  expect(aborted).toEqual({ error: { name: "ClientError", kind: "server", code: "cancelled" } });
  // Aborted before it started: still cancelled by the server.
  const early = await page.evaluate((s) => harness().wait(s, { output: { text: "never-printed" } }, 0), session);
  expect(early).toEqual({ error: { name: "ClientError", kind: "server", code: "cancelled" } });
  await page.context().close();
});

test("a terminal that opted in marks its session's done record seen on focus; one that did not leaves it", async () => {
  const report = "printf '\\033]7501;state=done:id=d\\033\\\\'; exec cat";
  const optedIn = await create("/bin/sh", "-c", report);
  const left = await create("/bin/sh", "-c", report);
  for (const id of [optedIn, left]) {
    await eventually("the done record", async () => ((await listed(id))?.attention?.state === "done" ? true : undefined));
  }
  const page = await opened();
  await page.evaluate(([a, b]) => (harness().mount("acks", a!, { ackOnFocus: true }), harness().mount("leaves", b!)), [optedIn, left]);
  await connectedTo(page, "acks");
  await connectedTo(page, "leaves");

  await page.focus("#leaves");
  await new Promise((resolve) => setTimeout(resolve, 300));
  expect((await listed(left))?.attention?.state).toBe("done");
  expect((await listed(optedIn))?.attention?.state).toBe("done");

  await page.focus("#acks");
  await eventually("the record seen", async () => ((await listed(optedIn))?.attention === undefined ? true : undefined));
  const record = JSON.parse(await cli("--json", "inspect", optedIn)).program_status.find((r: { id: string }) => r.id === "d");
  expect(record.seen).toBe(true);
  expect((await listed(left))?.attention?.state).toBe("done");
  await page.context().close();
});

test("the client's link, the latest values a late listener gets, and close rejecting what is pending", async () => {
  const session = await create("/bin/cat");
  const page = await opened();
  await eventually("the client connected", async () =>
    (await page.evaluate(() => harness().clientLinks())).at(-1)?.state === "connected" ? true : undefined,
  );
  expect(await page.evaluate(() => harness().clientLinks())).toEqual([
    { state: "connecting", attempt: 1 },
    { state: "connected", session: undefined },
  ]);
  await page.evaluate((s) => harness().mount("only", s), session);
  await connectedTo(page, "only");
  const latest = await page.evaluate(() => harness().latest("only"));
  expect(latest.link).toMatchObject({ state: "connected", session: { id: session } });
  expect(latest.scroll).toBe(0);
  expect(latest.status).toBe("attached");
  expect(await page.evaluate((s) => harness().closedWhileWaiting(s), session)).toEqual({ kind: "closed" });
  await eventually("the attachment closed", async () => ((await listed(session))?.attachments.length === 0 ? true : undefined));
});
