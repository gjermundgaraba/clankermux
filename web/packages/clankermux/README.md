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
| `attach(session)` | Detaches the current session and attaches `session`. |
| `create(options?)` | Creates a session at the current size, attaches it, and resolves with it. `options.onExit`: `"keep"` keeps it listed as `exited` when its program exits; `"remove"` removes it `limits.remove_grace` after. Without it the server's default (`remove`) applies. |
| `end()` | Hangs up the attached session's processes (SIGHUP). Also resolves when the session is not running (`exited`, `restored`): there is nothing to end. |
| `remove()` | Removes the attached session, which must be `exited` or `restored`; a running one rejects with `running`. |
| `report()` | Reports the container's size, the colours and the scheme again. |
| `paste(text)` | Pastes, bracketed when the program asked for it. |
| `dispose()` | Removes the listeners and terminates the worker, which drops its connection; requests in flight reject. |

`create`, `end` and `remove` reject with a `ClientError`:

- with `kind: "server"` and `server.code` when the server refuses;
- with `kind: "closed"` when there is no connection.

`end` and `remove` reject with a plain `Error` when no session is attached. Closing a pane for good is `end()` for a running session and `remove()` for one that has exited, or that a previous server left `restored`. With `onExit: "remove"` an ended session also removes itself.

## `onLink`

`onLink(link)` reports each change of the page's link to its session, through the connection and the attachment. `onStatus(text)` reports the same moments, and more, as text for display.

```ts
type Link =
  | { state: "connecting"; attempt: number }
  | { state: "connected"; session: Session | undefined }
  | { state: "ended"; reason: LinkEnd; code?: ErrorCode; message?: string; exit?: Exit; retry: boolean };
type LinkEnd = "closed" | "removed" | "stopping" | "exited" | "no_such_session" | "engine_mismatch" | "error";
```

`retry` means the worker reconnects or attaches again by itself, so a `connecting` or `connected` follows. Otherwise nothing follows until the page calls `attach` or `create`. After a connection-level end (the last four rows of the first table), those cannot work either, and only a new mount connects again.

| When | Link |
| --- | --- |
| A connection is opened: at mount, and at each reconnect | `connecting`, `attempt` from 1; it starts again at 1 once a server answers |
| A connection cannot be opened, or closes before `WELCOME` | nothing at once; the next `connecting` after a backoff |
| The connection closes | `ended closed`, `retry: true`, then `connecting` |
| The engine cannot be loaded | `ended error`, `retry: false` |
| The server refuses the handshake (`ERROR` before `WELCOME`, e.g. `version_mismatch`), or sends something else first | `ended error` with the code when there is one, `retry: false` |
| The server ends the connection (`ERROR` without an id, e.g. `protocol_error`) | `ended error` with the code, `retry: false` |
| `subscribe` fails on a live connection | `ended error`, `retry: false` |

Once connected:

| When | Link |
| --- | --- |
| Connected with no session to attach | `connected`, `session: undefined` |
| A session's screen is shown: a snapshot's first screen, a resume, again after a gap, a re-attach or a reconnect | `connected` with the session as `ATTACHED` described it; `status` tells `running`, `restored` or `exited` |
| The session shown has exited already | `connected`, then at once `ended exited` with its `exit`, `retry: false` |
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
