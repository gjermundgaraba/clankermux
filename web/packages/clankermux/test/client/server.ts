import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { connect as netConnect, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { StreamFramer } from "../../src/proto/index.ts";

import type { Transport } from "../../src/client/connection.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const binary = `${here}../../../../../target/debug/clankermux`;
if (!existsSync(binary)) throw new Error(`${binary} is missing: run cargo build first`);

export class StreamTransport implements Transport {
  private readonly framer = new StreamFramer(1 << 30);
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  constructor(private readonly socket: Socket) {
    socket.on("data", (chunk: Buffer) => {
      for (const frame of this.framer.feed(chunk)) this.onMessage?.(frame);
    });
    socket.on("close", () => this.onClose?.());
    socket.on("error", () => {});
  }

  static connect(path: string): Promise<StreamTransport> {
    return new Promise((resolve, reject) => {
      const socket = netConnect(path);
      socket.once("connect", () => resolve(new StreamTransport(socket)));
      socket.once("error", reject);
    });
  }

  send(bytes: Uint8Array): void {
    this.socket.write(bytes);
  }

  close(): void {
    this.socket.destroy();
  }
}

export interface Served {
  socket: string;
  wsUrl: string | undefined;
  stop(): Promise<void>;
}

export async function serve(ws?: { origin: string }): Promise<Served> {
  const dir = mkdtempSync(join(tmpdir(), "cmx-"));
  const socket = join(dir, "sock");
  const tls = `${here}fixtures/tls/`;
  const carrier = ws
    ? ["--ws", "127.0.0.1:0", "--origin", ws.origin, "--tls-cert", `${tls}cert.pem`, "--tls-key", `${tls}key.pem`]
    : [];
  const server = spawn(binary, ["--socket", socket, "serve", ...carrier], {
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, XDG_STATE_HOME: join(dir, "state") },
  });
  const stop = async () => {
    if (server.exitCode === null) await new Promise((resolve) => (server.once("exit", resolve), server.kill()));
    rmSync(dir, { recursive: true, force: true });
  };
  server.stderr!.on("data", (chunk: Buffer) => process.stderr.write(chunk));
  const wsUrl = ws
    ? await new Promise<string>((resolve, reject) => {
        let seen = "";
        server.stderr!.on("data", (chunk: Buffer) => {
          seen += chunk;
          const m = /listening on (wss:\/\/\S+)/.exec(seen);
          if (m) resolve(m[1]!);
        });
        server.once("exit", (code) => reject(new Error(`server exited: ${code}`)));
      })
    : undefined;
  const deadline = Date.now() + 10_000;
  for (;;) {
    try {
      (await StreamTransport.connect(socket)).close();
      return { socket, wsUrl, stop };
    } catch {
      if (Date.now() > deadline) throw new Error("server did not listen");
      await new Promise((r) => setTimeout(r, 20));
    }
  }
}
