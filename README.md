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
| `just check` | fmt, bindings, PROTOCOL.md's generated tables, clippy, Rust and web tests (the web package in the installed Chrome among them; `CHROME` names another), web builds; required for every commit |
| `just protocol` | renders PROTOCOL.md's tables (between `generated` marks) from the protocol types in `crates/proto` |
| `just release-build` | `target/release/clankermux` |
| `just dev-serve` | release server: state and socket in `target/dev/`, `wss://127.0.0.1:7438/` with a self-signed certificate, origins `http://app.clankerspace-dev.localhost:10094` and `http://127.0.0.1:8787` |
| `just pack-web` | `web/dist-packages/gjermundgaraba-clankermux-<version>.tgz`, checked, and `COMMIT` |
| `cd web/apps/client && pnpm build && pnpm serve` | demo page on `http://127.0.0.1:8787/?ws=wss://127.0.0.1:7438/`; after `just check`, which installs `web/` and builds the wasm |
| `target/release/clankermux --socket target/dev/sock create` | a session on the dev server |

## The command line

Every session's child gets `CLANKERMUX_SESSION` (its session's id) and `CLANKERMUX_SOCKET` (the server's socket, an absolute path), whatever `create`'s `env` says, so a command run inside a session needs no flags: `clankermux label building` labels the session it runs in.

A session argument is resolved in order: the full id; the label, ignoring case; four or more characters from the start or the end of the id. Left out, it is `$CLANKERMUX_SESSION`, when the command talks to the server in `$CLANKERMUX_SOCKET`. `list` shows the last six characters of each id. When the argument names no session or several, the candidates go to stderr (short id, label, status) and the exit code is 3. A command's socket is `--socket`, else `$CLANKERMUX_SOCKET`, else the default; `serve` ignores `$CLANKERMUX_SOCKET`.

| Exit code | Meaning |
| --- | --- |
| 0 | Done |
| 1 | Failed: the server refused the request, or the connection broke |
| 2 | Usage: arguments clap or the command could not use, or no session given outside a session |
| 3 | The session argument named no session, or several |
| 4 | The server cannot be reached |
| 124 | A wait timed out |
| 130 | A wait was cancelled with Ctrl-C |
| child's | A wait that ended with the session's exit (`wait --exit`, `create --wait`): its code, or 128 plus the signal's number; 1 for a `restored` session, which has none |

`send` writes text as typed (`--enter` presses Enter after it), named keys (`--keys ctrl+c up enter`, encoded as the program's modes expect), a paste (`--paste`) or stdin's bytes (`--bytes`). `wait` waits for the session to exit, its output to match a regex, a program status, the next prompt or a quiet spell, with `--timeout`; `send` takes the same flags (`--wait-match` and so on), and its wait starts before the bytes are written; a `--status` wait there counts only a status set after the send. The terminal's echo of what you typed is output too, so match on what only the program prints. With one argument, `send` takes it as TEXT for the session it runs in, and refuses one that names a session (give SESSION and TEXT). `create --wait` runs a program to its end; with `--json` it prints `{session, wait}`.

A program reports its status with OSC 7501 (PROTOCOL.md, "Program status"). `list` prints each session's attention after its status: the most urgent report that wants someone, `blocked` (with its kind, `blocked:permission`), then an unseen `error`, then an unseen `done`, then `working`. `inspect` shows every report, `done` and `error` ones with `seen`; `clankermux ack [SESSION [ID...]]` marks them seen, the named ones or all, and the mark survives parks and restarts.

`clankermux events` prints the sessions, then every event as a JSON line with its `seq`, which counts from 1 on each server start: session changes, program status, `bell`, `notification` (OSC 9 and 777), `progress` (OSC 9;4), `command_started` and `command_ended` (OSC 133 `C` and `D`, at their stream offsets) and attachments opening and closing. `events` first prints the server's incarnation and the latest `seq` on stderr. `events --after SEQ --incarnation ID` starts after that event, replaying the ones the server still holds (up to the latest 4096); when it holds too few, or `ID` names another server start, it says so on stderr and prints the sessions first. Without `--incarnation` it assumes the connected server's.

`read` takes `--format text|vt|html` and `--scope screen|history|alternate|last_command`; `last_command` needs a shell that marks its prompts and commands with OSC 133 (shell integration), and fails without one. `tail S [--after OFFSET] [--follow]` prints the session's output stream as plain text from a stream offset (by default the oldest byte kept) up to where the stream was when it began; `--follow` instead keeps going, with a `wait` for more output at the end of each page, until the stream ends.

`inspect` shows a session's attachments: who is attached (each connection's `HELLO` client), read-only or not, the size each last reported, and when each last typed. `clankermux detach S CHANNEL [--reason TEXT]` closes one of them by its `channel`; a CLI attach so closed prints the reason and exits 0, and a web page shows it and does not attach that session again by itself. `clankermux resize S [COLSxROWS] [--policy latest|smallest|fixed]` sets the size, the size policy, or both: `latest` (the default) takes the most recent report, `smallest` the smallest columns and rows among the attachments' reports, `fixed` only what `resize` sets.

`clankermux api list` lists the methods the server speaks, from its `describe` (the catalog PROTOCOL.md's tables are rendered from): each with its class (`read`, `mutation` or `stream`) and what it does. `clankermux api describe METHOD` shows one: every field of its params and result with its type and meaning, the error codes it can answer, and examples; `--json` prints the catalog entry with its JSON Schemas. `clankermux api call METHOD [JSON]` sends one request as it is (params `{}` when left out, `-` reads them from stdin) and prints the result as JSON; it assumes no session, a server error exits 1, or 3 for a session the server does not know, and Ctrl-C cancels a call that waits.

`clankermux skill show` (or `--skill`) prints a guide for agents: what a session is, the variables above, each command with an example, the exit codes, and the rules for sessions an agent did not create.

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

- **0.4.0**: the protocol describes itself, and the web package is a client. `describe` answers the catalog of methods, each with its class (`read`, `mutation` or `stream`), the JSON Schemas of its params and result, the error codes it can answer, and examples; the catalog also lists the events, the frames and the error codes, and is stamped with the protocol number and the release; `describe {method}` answers one. PROTOCOL.md's tables are rendered from the same catalog. WELCOME's `server.version` and the XTVERSION reply give the release (0.3.0 said 0.1.0). `read` names what to read and how: `format` `text`, `vt` or `html`, and `scope` `screen`, `history`, `alternate` or `last_command` (commands a shell marks with OSC 133 are kept as stream offset ranges, `Session.command` and `limits.commands`), with `truncated` when a reply was cut to fit; `tail {after}` pages the output stream from a byte offset, after the session exits too; new error `scope_unavailable`. Every `EVENT` carries `seq`, and `subscribe {after, incarnation}` replays the events the server kept (`limits.events`) after a cursor, answering `{oldest, latest, gap}`. New events: `bell`, `notification`, `progress`, `command_started`, `command_ended`, `attachment_opened` and `attachment_closed`. `done` and `error` records carry `seen` until `ack` marks them, and every session carries `attention`, its most urgent unseen record. `list` and `inspect` show each session's attachments (a server-wide `channel`, the client's name and version, read-only or not, its size, when it opened and last typed); `detach` closes another client's attachment with `CLOSED detached`; `resize` takes a size policy (`latest`, `smallest` or `fixed`), which `Session.size` names and which survives a restart; a session takes at most `limits.attachments` (16). Parks written by 0.3.0 load. The CLI gains `api list`, `api describe METHOD` and `api call METHOD [JSON]` (`clankermux skill show` points agents at `api describe`), `read --format/--scope`, `tail [--after N] [--follow]`, `ack`, an attention column in `list`, `events --after`, `detach` and `resize --policy`. The web package: `connect({url, worker, wasm, client?})` starts one worker and one WebSocket and resolves with a `Client` whose methods are the protocol's, failing with a `ClientError` that carries the server's code; an `AbortSignal` on `wait` or `send` cancels it on the server; `onLink` follows the connection, `onSessions` the session list and `onEvent` every event in order, replayed after a reconnect or reported as a gap; `client.mount({session, canvas, container, font, colors, scheme, readOnly?, ackOnFocus?})` draws a terminal as one attachment on that connection, so a page with many terminals holds one connection, and a lost connection resumes each terminal where it was; `describe()` and the catalog's types. `mountTerminal` is gone: a page calls `connect` once and `mount` per terminal. A 0.4.0 client needs a 0.4.0 server (`WELCOME.limits.attachments`); deploy together.
- **0.4.1**: the web package. Under the kitty keyboard protocol's disambiguation, which neovim asks for, a key's text that Shift made goes as typed (`:`, `"`, `|`), where it went as the layout's unshifted character with Shift, so neovim's command line could not be opened from a browser. The CLI was not affected. `terminal.onCopy(listener)` tells the page each copy to the clipboard as it ends: `{ok: true, source, chars}`, or `{ok: false, source, reason, chars?, message?}` with `reason` `refused` (the browser's error in `message`), `timeout` or `unavailable`. `source` is `release`, `keys`, `browser` or `program` (OSC 52). A release that selected nothing tells nothing. For direct users of the worker messages: a `select` release carries `copy`, the page's number for its copy, and `selection` answers with that number (`copy?: number`, was a boolean). The server is unchanged.
- **0.3.0**: the API's names and rules, with no aliases (PROTOCOL.md is version 1, redefined). `write`, `set_label` and `set_size` are now `send`, `label` and `resize`, and `resize` keeps a side left out. Every payload a client sends refuses fields the protocol does not define (`invalid_request`); what the server sends may still grow. Every child gets `CLANKERMUX_SESSION` and `CLANKERMUX_SOCKET`. The CLI resolves a session by id, label or either end of the id, defaults to the session it runs in, and exits 3 for a session it cannot pick and 4 for a server it cannot reach; `clankermux skill show` prints a guide for agents. `send` also takes text and named keys, encoded by the session's terminal against the program's modes, and a paste, and may carry a wait armed before its bytes are written; `wait` waits for an exit, an output match, a program status, a prompt or quiet, and `cancel` ends a wait. On the command line: `send --keys`, `--paste` and `--wait-*`, `wait`, `create --wait`, exit 124 for a timeout and the child's status for an exit. The web package's API is unchanged; its worker pastes with `send`, so a 0.2.0 page cannot paste into a 0.3.0 server, nor a 0.3.0 page into a 0.2.0 one.
- **0.2.0**: the web package for daily use. Mouse selection copied on release (word and line with double and triple click; Cmd+C and Ctrl+Shift+C copy again), scrollback with the wheel and Shift+PageUp/PageDown/Home/End (a mark while scrolled back, `onScroll`), input methods and dead keys through a textarea that takes the focus from the canvas (`focus()`), the session sized by the visible, focused client, and a reported scheme that follows the canvas's `color-scheme`. For direct users of `./engine`, `./renderer` and the worker messages: `Frame.scrollbar`, `KeyEventLike.keyCode` and `active` in the `init` and `viewport` messages are new and required. The server is unchanged; a test now covers its working-directory announcements.
- **0.1.1**: a running session parks within seconds of a change and when its last attachment closes, so a server killed without a signal restores it as it was; `serve --park-timing` sets the park times for tests. The web package's `create({ onExit })` (which now returns a promise), `end()`, `remove()` and `onLink`.
- **0.1.0**: the first release.

## The web package

`@gjermundgaraba/clankermux` exports the client at `.` (`connect`, `Client`, the `Terminal` a client mounts, and their types), the worker at `./worker` (`runWorker`, with no DOM types), the engine build at `./ghostty-vt.wasm`, and `./proto`, `./engine` and `./renderer` beneath them. A page holds one client, one worker and one WebSocket, whose methods are the protocol's (`create`, `list`, `send`, `wait`, `read`, `tail`, `ack`, ...), and mounts its terminals from it: each is one attachment on that connection. Its [README](web/packages/clankermux/README.md) describes the client's methods and events, the terminal's methods and the link changes `onLink` reports, and the terminal's keys, composed text, selection, scrollback, size between clients and colour scheme.

```ts
// worker.ts, started as a module worker
import { runWorker } from "@gjermundgaraba/clankermux/worker";
runWorker(self);

// page
import { connect } from "@gjermundgaraba/clankermux";
import wasm from "@gjermundgaraba/clankermux/ghostty-vt.wasm?url";
const client = await connect({
  worker: new Worker(new URL("./worker.ts", import.meta.url), { type: "module" }),
  wasm, url: "wss://127.0.0.1:7438/",
});
const terminal = client.mount({
  canvas, container, session,
  font: "14px monospace", colors: () => ({ fg: "#d8dee9", bg: "#282c34", cursor: "#ffffff" }),
});
```

From npm: `pnpm add @gjermundgaraba/clankermux`.
