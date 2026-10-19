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

## Keys and composed text

The terminal's input element is a textarea the mount places after the canvas: an input method composes only into an editable element. It is invisible, out of the tab order (`tabIndex = -1`) and ignores the pointer. Focusing the canvas (a click, `canvas.focus()`, a tab stop the page gives it) or calling `focus()` focuses the textarea, so `document.activeElement` is the textarea while the terminal has the keyboard: test for focus with `container.contains(document.activeElement)`, not `=== canvas`. Key events bubble from it through the container as they did from the canvas.

- Keys go to the engine's key encoder, which follows the program's modes (cursor keys, the kitty keyboard protocol). The browser keeps Cmd (Meta) chords and Ctrl+Shift+V, its paste.
- A key that belongs to an input method is left to it: one that is composing (`isComposing`), that it processes (`Process`, key code 229), or a dead key (`Dead`). The composition is shown at the cursor, in the terminal's colours, while it lasts, and its text is sent when it ends (`compositionend`). A key that ended a composition sends no release.
- Text inserted with no key that sent it (a dead key's character, the emoji picker, dictation) is sent as it is inserted (`beforeinput` `insertText`); a line break or a backward delete from such a source is sent as Enter or Backspace.
- Composed text goes to the program as UTF-8, as typed text does, not as a paste.

A page that holds input back (a pause) holds these events, which the terminal acts on: on the input element `keydown`, `keyup`, `beforeinput`, `compositionstart`, `compositionend`, `paste`, `beforecopy` and `copy`; on the canvas `mousedown` (focus and selection; the drag continues on `window` `mousemove`/`mouseup`) and `wheel` (which sends arrow keys on the alternate screen). macOS press-and-hold accents send the base letter on its keydown and then the accented one, as in xterm.js.

Verified in headless Chrome on macOS through CDP (`Input.imeSetComposition`, `Input.insertText`, `Input.dispatchKeyEvent`): a Japanese composition sent once at its end, nothing during it; a dead key (Option+E, then é) with no Alt+E sent; an emoji inserted with no key; plain keys sent once. Not verified by hand with a real input method, in Safari or in Firefox.

## `onSessions`

`onSessions(sessions, current)` reports every session on the server, and which one the page shows, at each change: a session created, exited or removed, and a `session_changed`, which the server sends when a session's label, size, size policy, attachments, title, working directory or attention changes. `attachments` is the list of open attachments in the order they opened, each `{channel, client: {name, version}, read_only, viewport?: {cols, rows}, since, last_input?}`: `channel` is unique on the server, `client` the connecting page or program's own name, `viewport` its latest size report, `last_input` when it last typed (input alone sends no `session_changed`). `size` is `{policy, owner?}`, `owner` being the channel whose report sets the size under `latest`. The title is the program's (OSC 0 or 2), the working directory the one it reports (OSC 7, as a path), each empty until set, and both cleared by a full reset. A status line takes them from the session whose `id` is `current`; the `session` of a `connected` link is the one `ATTACHED` described, and does not follow these changes.

A session's `attention`, when present, is the program status record that most wants the user (`{state, kind?, app?, msg?, record}`: `blocked`, then an unseen `error`, then an unseen `done`, then `working`), so a badge needs no request per session; a `session_changed` follows each change of it.

## `onEvent` (provisional)

`onEvent(event)` reports each server event that carries the attached session's `id`: `bell`, `notification`, `progress`, `command_started`, `command_ended`, `attachment_opened`, `attachment_closed`, `program_status_changed`, `program_status_removed` and `session_removed`, as PROTOCOL.md describes them, `seq` included. The events that carry a whole session reach `onSessions` instead. It is provisional: when the connection and the mounted terminal become separate objects, events move to the connection's object.

## `onLink`

`onLink(link)` reports each change of the page's link to its session, through the connection and the attachment. `onStatus(text)` reports the same moments, and more, as text for display.

```ts
type Link =
  | { state: "connecting"; attempt: number }
  | { state: "connected"; session: Session | undefined }
  | { state: "ended"; reason: LinkEnd; code?: ErrorCode; message?: string; exit?: Exit; retry: boolean };
type LinkEnd = "closed" | "removed" | "stopping" | "detached" | "exited" | "no_such_session" | "engine_mismatch" | "error";
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
| `CLOSED detached` (another client's `detach`) | `ended detached`, `retry: false`, `message` the reason it gave, if any; the worker forgets the session, so a reconnect does not take it back |
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

## Selection and copy

The mouse selects on the canvas, and what it selects is copied to the clipboard when the button comes up, with no keystroke:

- a drag selects from the pressed cell to the pointer's, by cells (the half of a cell the pointer passed decides the last one);
- a double click selects a word and a triple click a line (a soft-wrapped line whole), and a drag after them extends by words or lines;
- a drag that leaves the canvas goes on, clamped to its edges; it does not scroll the history.

The text is the engine's: soft-wrapped lines are joined, trailing blanks on each line trimmed, and lines end with `\n`. The browser's copy (Cmd+C, or the Copy menu) copies the selection again, through a `copy` event on the input element, and so does Ctrl+Shift+C (without a selection Ctrl+Shift+C goes to the program as a key, and Ctrl+C is always the program's). `selection()` returns the selected text. Paste is unchanged.

The selection is the engine's, drawn inverted, and the engine keeps it on its cells:

- output that scrolls the selected rows up moves the selection with them;
- output that writes over the selected cells, or erases them (`clear`), leaves it on those cells; the text it copies stays the text selected at the release;
- scrolling the view moves it with the text; it can reach into the history and beyond the view;
- a resize reflows it with the text.

It is cleared by a click (a press that selects nothing), by input sent to the session (a key that sends something, a paste, composed text; not a modifier alone), by a switch to the alternate screen (it is not back when the program leaves it), by a full reset (RIS), and by a new screen (attaching, a gap).

The copy on release starts within `mouseup`, as Safari requires: `navigator.clipboard.write` with a `ClipboardItem` whose text arrives when the worker has it, a few milliseconds later; when nothing was selected it fails and the clipboard is left alone. Where `ClipboardItem` is missing it is `writeText` once the text arrives, which a browser that wants the gesture still running may refuse; Cmd+C then copies. Verified in headless Chrome only. A `beforecopy` listener lets WebKit offer Copy on the empty input element while there is a selection. Where the browser keeps Ctrl+Shift+C for itself (a developer tools shortcut), Cmd+C or the Copy menu copies. Programs that ask for mouse reports get none from this client, so the mouse always selects.

## Scrollback

The terminal holds the session's history (the server sends what it keeps with the screen), and the view scrolls through it:

- the wheel over the canvas, in whole rows (a trackpad's pixels add up until they make one; Ctrl+wheel stays the browser's zoom);
- Shift+PageUp and Shift+PageDown, a screen at a time; Shift+Home and Shift+End, to the top of the history and back to the live screen.

Output while the view is scrolled back leaves it where it is. Any input that sends something (a key, a paste, composed text) brings it back to the live screen first; a modifier alone does not. The alternate screen of full-screen programs (`vim`, `less`) has no history: there Shift+PageUp and the rest go to the program as keys, and the wheel sends the up and down arrow keys while the program leaves alternate scroll (mode 1007) on, as it is by default, or nothing.

While the view is scrolled back the canvas draws a mark at its right edge, the view's share of the history and its place in it. `onScroll(rows)` reports the rows between the bottom of the view and the live screen's at each change: above 0 when scrolled back (it grows as output arrives meanwhile), 0 at the live screen.

## Size between clients

A session has one size, and under the default size policy, `latest`, the server applies the latest report (PROTOCOL.md, "Size"); under `smallest` it takes the smallest columns and rows among the attachments' reports, and under `fixed` only a `resize` request changes it, the reports being recorded but not applied. The terminal reports its container's size, its colours and its scheme only while its page is visible and its window has the focus (`document.visibilityState === "visible"` and `document.hasFocus()`). It reports again when the page is shown or its window focused, even at an unchanged size, and when the session was resized to another client's size while the page kept its focus (the user went to another machine and came back), at the page's next input: a key or text that sends something, a paste, a click on the canvas, the wheel. So under `latest`, with the same session open in several tabs or on several machines, the session takes the size of the one you look at, and a hidden or unfocused one never resizes it: its `OPEN` carries no viewport, it sends no `VIEWPORT`, and its canvas shows the session's size, clipped or with a margin. `report()` follows the same rule. `create()` makes the session at the container's current size, focused or not. A headless browser counts only its newest page as visible and focused, and a terminal in an iframe has the focus only once the frame does; such a page does not size the session until then.

## Colour scheme

The terminal reports a colour scheme, `light` or `dark`, with its colours; programs read it through the colour scheme query (mode 2031). By default it is the canvas's own:

- the scheme the canvas's computed `color-scheme` names, when it names one of `light` and `dark` (`color-scheme: dark`, `only light`);
- otherwise (`normal`, `light dark`) the page's `prefers-color-scheme`.

It is reported again when the preference changes, and when an attribute changes on the canvas or an ancestor (a `class`, `style` or `data-*` that pins the page's scheme) and the scheme or the colours differ from the last report. A stylesheet swapped without an attribute change is not noticed; call `report()` after it.

To pin it, set `color-scheme` on the canvas (or an ancestor) in CSS, or pass `scheme: () => "dark"`, or a function of your own; it is read on every report.
