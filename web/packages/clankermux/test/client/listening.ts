import type { Readable } from "node:stream";

export function listeningOn(stderr: Readable): Promise<string> {
  return new Promise((resolve, reject) => {
    let seen = "";
    stderr.on("data", (chunk: Buffer) => {
      seen += chunk;
      const m = /listening on (wss:\/\/\S+)\n/.exec(seen);
      if (m) resolve(m[1]!);
    });
    stderr.once("end", () => reject(new Error(`server exited before it listened: ${JSON.stringify(seen)}`)));
  });
}
