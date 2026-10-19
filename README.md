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

## The command line

A session argument is resolved in order: the full id; the label, ignoring case; four or more characters from the start or the end of the id. Left out, it is `$CLANKERMUX_SESSION`. `list` shows the last six characters of each id. When the argument names no session or several, the candidates go to stderr (short id, label, status) and the exit code is 3. A command's socket is `--socket`, else `$CLANKERMUX_SOCKET`, else the default; `serve` ignores `$CLANKERMUX_SOCKET`.

| Exit code | Meaning |
| --- | --- |
| 0 | Done |
| 1 | Failed: the server refused the request, or the connection broke |
| 2 | Usage: arguments clap or the command could not use, or no session given outside a session |
| 3 | The session argument named no session, or several |
| 4 | The server cannot be reached |

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

- **0.2.0**: the web package for daily use. Mouse selection copied on release (word and line with double and triple click; Cmd+C and Ctrl+Shift+C copy again), scrollback with the wheel and Shift+PageUp/PageDown/Home/End (a mark while scrolled back, `onScroll`), input methods and dead keys through a textarea that takes the focus from the canvas (`focus()`), the session sized by the visible, focused client, and a reported scheme that follows the canvas's `color-scheme`. For direct users of `./engine`, `./renderer` and the worker messages: `Frame.scrollbar`, `KeyEventLike.keyCode` and `active` in the `init` and `viewport` messages are new and required. The server is unchanged; a test now covers its working-directory announcements.
- **0.1.1**: a running session parks within seconds of a change and when its last attachment closes, so a server killed without a signal restores it as it was; `serve --park-timing` sets the park times for tests. The web package's `create({ onExit })` (which now returns a promise), `end()`, `remove()` and `onLink`.
- **0.1.0**: the first release.

## The web package

`@gjermundgaraba/clankermux` exports the page mount at `.` (`mountTerminal` and its types), the worker at `./worker` (`runWorker`, with no DOM types), the engine build at `./ghostty-vt.wasm`, and `./proto`, `./engine` and `./renderer` beneath them. Its [README](web/packages/clankermux/README.md) describes the mounted terminal's methods (`create`, `end`, `remove`, ...), the link changes `onLink` reports, and the terminal's keys, composed text, selection, scrollback, size between clients and colour scheme.

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
