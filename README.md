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
| `just pack-web` | `web/dist-packages/gjermundgaraba-clankermux-<version>.tgz`, checked, and `COMMIT` |
| `cd web/apps/client && pnpm build && pnpm serve` | demo page on `http://127.0.0.1:8787/?ws=wss://127.0.0.1:7438/`; after `just check`, which installs `web/` and builds the wasm |
| `target/release/clankermux --socket target/dev/sock create` | a session on the dev server |

## Parks

`serve` writes each running session's screen, scrollback and state under `$XDG_STATE_HOME/clankermux/sessions/` (PROTOCOL.md, "Persisted state"):

- within 5 s of its output (or input, a resize, a label) stopping (2 s when it has not parked in the last 5 s), and at least every 30 s while it continues, but no more than once every 5 s;
- when its last attachment closes;
- every 5 minutes;
- on SIGTERM, SIGINT or SIGHUP, before it exits.

A server that dies without a signal (SIGKILL, a machine powered off) starts again with each session `restored` as of its last park. `serve --park-timing INTERVAL,QUIET,GAP,BUSY` sets the times in seconds, fractions allowed (default `300,2,5,30`): every INTERVAL; QUIET after the last change, or BUSY after the first since the last park, whichever is sooner; never within GAP of the last park. A test can kill a server soon after typing and find the line parked: `clankermux serve --park-timing 300,0.2,0.2,1`.

## Release

1. Bump `version` in `crates/clankermux/Cargo.toml` and `web/packages/clankermux/package.json` (`scripts/version.sh` checks they agree), push `main` and wait for `check.yml`.
2. `git tag v<version> && git push origin v<version>`: `release.yml` attaches `clankermux-<version>-{x86_64,aarch64}-unknown-linux-gnu` and `SHA256SUMS` to a GitHub release; `npm.yml` runs `just pack-web` and publishes the tarball as `@gjermundgaraba/clankermux@<version>` through npm trusted publishing, skipping a version npm already has.

npm registers a trusted publisher only on a package that exists, so the first version is published by hand, logged in to npm as the owner of the `@gjermundgaraba` scope, at the tagged commit with a clean tree:

```sh
just pack-web                    # COMMIT names the tag's commit, without -dirty
npm publish web/dist-packages/gjermundgaraba-clankermux-0.1.0.tgz --access public --dry-run
npm publish web/dist-packages/gjermundgaraba-clankermux-0.1.0.tgz --access public
npm trust github @gjermundgaraba/clankermux --repo gjermundgaraba/clankermux --file npm.yml --allow-publish
```

The last line, or the package's settings on npmjs.com, names this repository and `npm.yml` as its trusted publisher.

### Versions

- **0.1.1**: a running session parks within seconds of a change and when its last attachment closes, so a server killed without a signal restores it as it was; `serve --park-timing` sets the park times for tests. The web package's `create({ onExit })` (which now returns a promise), `end()`, `remove()` and `onLink`.
- **0.1.0**: the first release.

## The web package

`@gjermundgaraba/clankermux` exports the page mount at `.` (`mountTerminal` and its types), the worker at `./worker` (`runWorker`, with no DOM types), the engine build at `./ghostty-vt.wasm`, and `./proto`, `./engine` and `./renderer` beneath them. Its [README](web/packages/clankermux/README.md) describes the mounted terminal's methods (`create`, `end`, `remove`, ...) and the link changes `onLink` reports.

```ts
// worker.ts, started as a module worker
import { runWorker } from "@gjermundgaraba/clankermux/worker";
runWorker(self);

// page
import { mountTerminal } from "@gjermundgaraba/clankermux";
import wasm from "@gjermundgaraba/clankermux/ghostty-vt.wasm?url";
const terminal = mountTerminal({
  worker: new Worker(new URL("./worker.ts", import.meta.url), { type: "module" }),
  canvas, container, wasm, url: "wss://127.0.0.1:7438/", session,
  font: "14px monospace", colors: () => ({ fg: "#d8dee9", bg: "#282c34", cursor: "#ffffff" }),
});
```

From npm: `pnpm add @gjermundgaraba/clankermux`.
