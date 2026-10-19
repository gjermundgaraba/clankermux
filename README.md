# clankermux

A terminal multiplexer server with CLI and browser clients; the wire protocol is [PROTOCOL.md](PROTOCOL.md).

## Prerequisites

| Tool | Version |
| --- | --- |
| [mise](https://mise.jdx.dev) | installs zig 0.16 and just from `mise.toml` (`mise install`) |
| Rust | 1.98 |
| pnpm | 12 (`web/package.json`), Node 24+ |

## Commands

| Command | Does |
| --- | --- |
| `just check` | fmt, bindings, clippy, Rust and web tests, web builds; required for every commit |
| `just release-build` | `target/release/clankermux` |
| `just dev-serve` | release server: state and socket in `target/dev/`, `wss://127.0.0.1:7438/` with a self-signed certificate, origins `http://app.clankerspace-dev.localhost:10094` and `http://127.0.0.1:8787` |
| `just pack-web` | `web/dist-packages/clankermux-{proto,engine,renderer,client}-<version>.tgz` and `COMMIT` |
| `cd web/apps/client && pnpm build && pnpm serve` | demo page on `http://127.0.0.1:8787/?ws=wss://127.0.0.1:7438/`; after `just check`, which installs `web/` and builds the wasm |
| `target/release/clankermux --socket target/dev/sock create` | a session on the dev server |

## Release

1. Bump `version` in `crates/clankermux/Cargo.toml` and `web/packages/*/package.json` (`scripts/version.sh` checks they agree).
2. `git tag v<version> && git push --tags`: `release.yml` attaches `clankermux-<version>-{x86_64,aarch64}-unknown-linux-gnu` and `SHA256SUMS`.
3. `just pack-web`, then `npm publish <tarball>` for proto, engine, renderer, client.

## Using the packages before a release

In the consumer, depend on `0.1.0` of the packages it imports and override all four to the tarballs (their dependencies on each other included):

```yaml
# pnpm-workspace.yaml
overrides:
  "@clankermux/proto": file:<clankermux>/web/dist-packages/clankermux-proto-0.1.0.tgz
  "@clankermux/engine": file:<clankermux>/web/dist-packages/clankermux-engine-0.1.0.tgz
  "@clankermux/renderer": file:<clankermux>/web/dist-packages/clankermux-renderer-0.1.0.tgz
  "@clankermux/client": file:<clankermux>/web/dist-packages/clankermux-client-0.1.0.tgz
```

```ts
// worker.ts, started as a module worker
import { runWorker } from "@clankermux/client/worker";
runWorker(self);

// page
import { mountTerminal } from "@clankermux/client";
import wasm from "@clankermux/engine/ghostty-vt.wasm?url";
const terminal = mountTerminal({
  worker: new Worker(new URL("./worker.ts", import.meta.url), { type: "module" }),
  canvas, container, wasm, url: "wss://127.0.0.1:7438/", session,
  font: "14px monospace", colors: () => ({ fg: "#d8dee9", bg: "#282c34", cursor: "#ffffff" }),
});
```

After `just pack-web` again, reinstall in the consumer (`pnpm install --force`).
