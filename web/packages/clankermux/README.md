# @gjermundgaraba/clankermux

The browser client of [clankermux](https://github.com/gjermundgaraba/clankermux): a terminal on a canvas, its engine and connection in a module worker, speaking the clankermux protocol over the server's TLS WebSocket carrier.

| Subpath | Exports |
| --- | --- |
| `.` | `mountTerminal` and its types, `ClientError` |
| `./worker` | `runWorker`, for the worker module; no DOM types |
| `./ghostty-vt.wasm` | the engine build the server's engine id names |
| `./proto`, `./engine`, `./renderer` | the protocol messages, the engine and the canvas renderer |

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
  onLink: (link) => console.log(link),
});
```

## `MountedTerminal`

| Method | Does |
| --- | --- |
| `attach(session)` | Detaches the current session and attaches `session`; after a connection-level end (below), connects again first. |
| `create(options?)` | Creates a session at the current size, attaches it, and resolves with it. `options.onExit`: `"keep"` keeps it listed as `exited` when its program exits; `"remove"` removes it `limits.remove_grace` after. Without it the server's default (`remove`) applies. |
| `end()` | Hangs up the attached session's processes (SIGHUP). Also resolves when the session is not running (`exited`, `restored`): there is nothing to end. |
| `remove()` | Removes the attached session, which must be `exited` or `restored`; a running one rejects with `running`. |
| `report()` | Reports the container's size, the colours and the scheme again. |
| `paste(text)` | Pastes, bracketed when the program asked for it. |
| `dispose()` | Removes the listeners and terminates the worker, which drops its connection; requests in flight reject. |

`create`, `end` and `remove` need a connection: call them after a `connected` link. They reject with a `ClientError`:

- with `kind: "server"` and `server.code` when the server refuses;
- with `kind: "closed"` when there is no connection, or after `dispose()`;
- with `kind: "protocol"` when the server sent a malformed frame meanwhile (the connection then ends with `ended closed`, `retry: true`).

`end` and `remove` reject with a plain `Error` when no session is attached. Since 0.1.1 `create` returns this promise, where it returned nothing and dropped a request it could not make; a caller that ignores it should `.catch` it. Closing a pane for good is `end()` for a running session and `remove()` for one that has exited, or that a previous server left `restored`. With `onExit: "remove"` an ended session also removes itself.

## `onSessions`

`onSessions(sessions, current)` reports every session on the server, and which one the page shows, at each change: a session created, exited or removed, and a `session_changed`, which the server sends when a session's label, size, attachments, title or working directory changes. The title is the program's (OSC 0 or 2), the working directory the one it reports (OSC 7, as a path), each empty until set, and both cleared by a full reset. A status line takes them from the session whose `id` is `current`; the `session` of a `connected` link is the one `ATTACHED` described, and does not follow these changes.

## `onLink`

`onLink(link)` reports each change of the page's link to its session, through the connection and the attachment. `onStatus(text)` reports the same moments, and more, as text for display.

```ts
type Link =
  | { state: "connecting"; attempt: number }
  | { state: "connected"; session: Session | undefined }
  | { state: "ended"; reason: LinkEnd; code?: ErrorCode; message?: string; exit?: Exit; retry: boolean };
type LinkEnd = "closed" | "removed" | "stopping" | "exited" | "no_such_session" | "engine_mismatch" | "error";
```

`retry` means the worker reconnects or attaches again by itself, so a `connecting` or `connected` follows. Otherwise nothing follows until the page calls `attach` or `create`. After a connection-level end (the last three rows of the first table) there is no connection: `create` rejects, and `attach` connects again (`connecting` from attempt 1), so "Retry" can be `attach`. An engine that failed to load needs a new mount.

| When | Link |
| --- | --- |
| A connection is opened: at mount, and at each reconnect | `connecting`, `attempt` from 1; it starts again at 1 once a server answers |
| A connection cannot be opened, or closes before `WELCOME` | nothing at once; the next `connecting` after a backoff |
| The connection closes | `ended closed`, `retry: true`, then `connecting` |
| The engine cannot be loaded | `ended error`, `retry: false` |
| The server refuses the handshake (`ERROR` before `WELCOME`, e.g. `version_mismatch`), or sends something else first | `ended error` with the code when there is one, `retry: false` |
| The server ends the connection (`ERROR` without an id, e.g. `protocol_error`) | `ended error` with the code, `retry: false` |
| `subscribe` fails on a live connection | `ended error`, `retry: false`; the worker closes the connection |

Once connected:

| When | Link |
| --- | --- |
| Connected with no session to attach | `connected`, `session: undefined` |
| A session's screen is shown: a snapshot's first screen, a resume, again after a gap, a re-attach or a reconnect | `connected` with the session as `ATTACHED` described it; `status` tells `running`, `restored` or `exited` |
| A snapshot shows a session that has exited already | `connected`, then at once `ended exited` with its `exit`, `retry: false` (a resume onto such a session gets its `EXITED`, the next row, instead) |
| The session's program exits (`EXITED`) | `ended exited` with `exit`, `retry: false` |
| `CLOSED removed` | `ended removed`, `retry: false`; the worker forgets the session |
| `CLOSED stopping` | `ended stopping`, `retry: true`; the connection's close follows, and the reconnect attaches the session again (as `restored`) |
| `OPEN` refused, or the attachment ended, with `no_such_session` or `engine_mismatch` | `ended` with that reason and code, `retry: false` |
| The attachment ended with `invalid_request` | `ended error`, `retry: false` |
| The attachment ended with another error (`too_slow`, a snapshot that failed, `internal`) | `ended error` with the code, `retry: true`; the worker attaches again after a backoff |
| The stream lost its place (an `OUTPUT` at an unexpected offset) | `ended error`, `retry: true`; the worker attaches again afresh |
| Any other fault in the attachment's frames | `ended error`, `retry: false` |

Nothing fires for:

- the errors that keep an attachment (`read_only`, `not_running`, `input_overflow`);
- a gap, until its new screen fires `connected`;
- `attach` or `create` before their screen;
- `dispose`.

`reason: "closed"` is the connection's close. The worker detaches without waiting for the server's `CLOSED closed`, so that frame never shows up as a link change.

"The server ended an answered connection with an error, and nothing will reconnect" is `ended` with `reason: "error"` and `retry: false` after a `connected`, with no `connecting` after it. An attached session is the latest `connected` with a `session`.

## Colour scheme

The terminal reports a colour scheme, `light` or `dark`, with its colours; programs read it through the colour scheme query (mode 2031). By default it is the canvas's own:

- the scheme the canvas's computed `color-scheme` names, when it names one of `light` and `dark` (`color-scheme: dark`, `only light`);
- otherwise (`normal`, `light dark`) the page's `prefers-color-scheme`.

It is reported again when the preference changes, and when an attribute changes on the canvas or an ancestor (a `class`, `style` or `data-*` that pins the page's scheme) and the scheme or the colours differ from the last report. A stylesheet swapped without an attribute change is not noticed; call `report()` after it.

To pin it, set `color-scheme` on the canvas (or an ancestor) in CSS, or pass `scheme: () => "dark"`, or a function of your own; it is read on every report.
