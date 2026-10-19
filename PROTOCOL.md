# clankermux protocol, version 1

A terminal multiplexer daemon with flat sessions, one PTY each. Clients and
server embed the same pinned libghostty-vt build. A client attaches by
decoding a Ghostty binary snapshot, then feeds raw PTY bytes to its own
terminal. The server is the only writer to a PTY.

## Conventions

- Integers in frame headers and raw payloads are unsigned little-endian.
- JSON payloads are UTF-8 objects. Strict in, open out: a payload a
  client sends (`HELLO`, `REQUEST` and every method's `params`, `OPEN`,
  `VIEWPORT`, and the objects inside them) holds only the fields this
  document defines, and the server refuses any other with
  `invalid_request`. A payload the server sends may gain fields; a client
  ignores the ones it does not know.
- `base64` fields are standard base64 with padding.
- Byte sizes are in bytes. `MiB` is 1,048,576 bytes. Durations in JSON
  are integer milliseconds.
- The engine id is an opaque string naming the libghostty-vt build, compared
  for exact equality. Recommended form: `ghostty-vt@<upstream commit>`, with
  any suffix for local changes.

## Framing

Every frame is a 10-byte header followed by `length` payload bytes.

| Offset | Size | Field     | Meaning                                         |
| ------ | ---- | --------- | ----------------------------------------------- |
| 0      | 1    | `type`    | Frame type, see below                           |
| 1      | 1    | `flags`   | Reserved, send 0, ignore on receipt             |
| 2      | 4    | `channel` | 0 for control; an attachment otherwise          |
| 6      | 4    | `length`  | Payload length, at most `limits.max_frame`      |

The high bit of `type` is set on frames the server sends.

| Type   | Direction | Channel | Payload   | Name             |
| ------ | --------- | ------- | --------- | ---------------- |
| `0x01` | client    | 0       | JSON      | `HELLO`          |
| `0x02` | client    | 0       | JSON      | `REQUEST`        |
| `0x10` | client    | n       | JSON      | `OPEN`           |
| `0x11` | client    | n       | raw       | `INPUT`          |
| `0x12` | client    | n       | JSON      | `VIEWPORT`       |
| `0x13` | client    | n       | empty     | `CLOSE`          |
| `0x81` | server    | 0       | JSON      | `WELCOME`        |
| `0x82` | server    | 0       | JSON      | `REPLY`          |
| `0x83` | server    | 0       | JSON      | `EVENT`          |
| `0x8F` | server    | 0 or n  | JSON      | `ERROR`          |
| `0x90` | server    | n       | JSON      | `ATTACHED`       |
| `0x91` | server    | n       | raw       | `SNAPSHOT`       |
| `0x92` | server    | n       | empty     | `SNAPSHOT_READY` |
| `0x93` | server    | n       | empty     | `SNAPSHOT_END`   |
| `0x94` | server    | n       | raw       | `OUTPUT`         |
| `0x95` | server    | n       | JSON      | `RESIZED`        |
| `0x96` | server    | n       | JSON      | `EXITED`         |
| `0x97` | server    | n       | JSON      | `GAP`            |
| `0x99` | server    | n       | JSON      | `CLOSED`         |

Channels `1` to `2^32-1` are chosen by the client, one per attachment. A
channel is in use from `OPEN` until the server's `CLOSED`, or an `ERROR`
that ends the attachment (see Errors).

Connection errors: an unknown type, a length above `limits.max_frame`, a
type on the wrong channel (`HELLO` or `REQUEST` on `n`, an attachment type
on 0), a JSON payload above `limits.control_body`, any frame but `HELLO`
before `WELCOME`, or a second `HELLO`. The server sends `ERROR
protocol_error` on channel 0 and closes the connection. A frame other
than `OPEN` on a channel that is not in use is dropped.

## Handshake

The client sends `HELLO` within `limits.hello_timeout` of connecting, else
the server closes the connection. The server answers `WELCOME`, or `ERROR
version_mismatch` and then closes the connection. A `HELLO` that is not a
JSON object is a `protocol_error`; one of version 1 with a field missing,
mistyped or undefined gets `ERROR invalid_request` without `id`, and the
connection closes.

`HELLO`

| Field          | Type     | Meaning                                    |
| -------------- | -------- | ------------------------------------------ |
| `version`      | int      | 1                                          |
| `client`       | object   | `{name, version}`, informational           |
| `engine`       | string?  | Engine id; absent for control-only clients |
| `capabilities` | string[] | Capability ids; version 1 defines none     |

`WELCOME`

| Field          | Type     | Meaning                                  |
| -------------- | -------- | ---------------------------------------- |
| `version`      | int      | 1                                        |
| `server`       | object   | `{name, version}`                        |
| `engine`       | string   | Engine id of the server                  |
| `incarnation`  | string   | UUID, new on every server start          |
| `capabilities` | string[] | Unknown ids are ignored                  |
| `limits`       | object   | See Limits                               |

The engine id is compared at `OPEN`, not at `HELLO`.

## Control

`REQUEST` is `{id, method, params}`. The server answers `REPLY {id, result}`
or `ERROR {id, code, message}`. `id` is a client-chosen integer, not reused
on the connection while its request is pending. Requests on a connection
are processed in order, and replies and events are sent in the order the
server produced them. The exception is a wait: a `wait`, or a `send` with
`wait`, holds its id until the wait ends, while the requests after it are
processed and answered.

| Method      | Params                                              | Result                  |
| ----------- | --------------------------------------------------- | ----------------------- |
| `create`    | `label?, argv?, cwd?, env?, cols?, rows?, on_exit?` | Session                 |
| `list`      | `after?`                                            | Page                    |
| `inspect`   | `session`                                           | Session with records    |
| `end`       | `session`                                           | `{}`                    |
| `signal`    | `session, signal`                                   | `{}`                    |
| `remove`    | `session`                                           | `{}`                    |
| `label`     | `session, label`                                    | Session                 |
| `resize`    | `session, cols?, rows?`                             | `{}`                    |
| `read`      | `session, scrollback?`                              | Read                    |
| `send`      | `session, text? \| keys? \| data?, paste?, wait?`    | Sent                    |
| `wait`      | `session, until, after?, timeout_ms?`               | Waited                  |
| `cancel`    | `id`                                                | `{cancelled}`           |
| `subscribe` |                                                     | Page                    |

- `create`: `argv` defaults to the user's login shell, `cwd` to the server
  user's home directory, `cols` to 80, `rows` to 24, `on_exit` to `remove`.
  `env` is an object of additions and overrides; the server sets `TERM` to
  `xterm-ghostty` unless overridden. It also sets `CLANKERMUX_SESSION` to
  the new session's id and `CLANKERMUX_SOCKET` to the absolute path of its
  Unix socket, which a server always has (see Carriers), whatever `env`
  says: these two are managed and win over `env`. A client run inside the
  session finds its server and its session from them. The child runs in a
  new session with the PTY as its controlling terminal.
- `end` sends SIGHUP to the session's processes. `signal` sends the named
  signal (`HUP`, `INT`, `TERM`, `KILL`, ...) to them. The targets are the
  child's process group while it exists, and otherwise every process that
  holds the PTY slave open. Both reply at once; the exit arrives as an
  event. Both fail with `not_running` unless `status` is `running`, and
  with `signal_failed` when no target could be signalled.
- `remove` deletes a session whose `status` is `exited` or `restored`,
  including its files. Fails with `running` otherwise. It replies at once;
  open attachments get `CLOSED removed`.
- `resize` counts as a viewport report applied at once, without
  coalescing (see Size). A side left out keeps the session's; one of
  `cols` and `rows` is required. Fails with `not_running` unless `status`
  is `running`.
- `list` and `subscribe` return a Page: `{sessions: Session[], next?}`.
  Sessions are ordered by `id`; at most `limits.page` per reply; `next` is
  the last id when more remain, to pass as `after`. Sessions carry no
  program status records; use `inspect` once per session.
- `read`: `scrollback` is a line count, default 0. `text` is the last
  `scrollback` history lines followed by the screen, one logical line per
  newline, with soft-wrapped rows joined and trailing blanks trimmed. The
  oldest lines are dropped to keep the reply under `limits.max_frame`.
  Works for every status.
- `send` takes exactly one of `text` (UTF-8, written as typed), `keys`
  (key names, see Keys) and `data` (base64 bytes). With `paste: true`,
  `text` or `data` is encoded as the engine encodes a paste: control
  bytes that could end it or run a command (ESC, NUL, DEL, ...) become
  spaces, and the whole is bracketed (`ESC [200~` ... `ESC [201~`) when
  the session's terminal has mode 2004 set, else its newlines become
  carriage returns. The bytes are written to the PTY as one unit. The
  reply, Sent, comes once they are written. With `wait`
  (`{until, timeout_ms?}`), a wait as `wait` describes it is armed at the
  session's cursor before the bytes are queued, so it sees everything the
  program writes in answer, and the reply comes when it ends, carrying
  its result. Fails with `not_running` unless `status` is `running`, and
  with `invalid_request`, writing nothing, for an unknown key (named in
  the message), `keys` with `paste`, or an invalid `wait`. Cancelling a
  `send` stops its wait, not its bytes.
- `wait` answers when one of the facts in `until` holds at or after the
  cursor `after` (`{offset, marker}`, default the session's cursor when
  the request is handled; not past it, else `invalid_request`). A fact
  that already holds there answers at once. `until` names one or more:
  - `exited: true`: the stream has ended (`restored`: at once, without
    `exit`);
  - `output: {text}` or `output: {regex}` (Rust `regex` syntax, on
    bytes): the stream from `after` on, as plain text: printable
    characters, newlines and tabs, with escape sequences, carriage
    returns and other controls left out, searched from `after.offset` or
    the oldest retained byte when that is gone. Text more than 512 KiB
    behind the newest may be dropped from the search;
  - `status: {states, record?}`: a program status record, or the record
    with id `record`, is in one of `states` (`idle`, `working`, `done`,
    `blocked`, `error`);
  - `prompt: true`: an OSC 133 `A` in output that ends after
    `after.offset`;
  - `quiet_ms: n`: no output for `n` ms, counted from the later of the
    request and the last output.

  When several hold at once, the first of `output`, `prompt`, `status`,
  `exited` and `quiet_ms` wins. Records do not change once the stream has
  ended, so a wait on `status` alone there that does not already hold
  ends only with `timeout_ms` or `cancel`.
  `timeout_ms` (none: no limit) ends the wait with reason `timeout`, a
  result, not an error. Fails with `no_such_session`, also when the
  session is removed while waiting, and with `not_running` when the
  stream has ended (or the session is `restored`) and nothing matched for
  a wait on `output`, `prompt` or `quiet_ms`.
- `cancel` ends the pending `wait`, or `send` with `wait`, whose request
  id is `id` on this connection: it gets `ERROR cancelled`, and then the
  `cancel` replies `{cancelled: true}`. After that reply the cancelled
  request sends nothing more. An `id` that is not pending replies
  `{cancelled: false}`.
- `subscribe` takes `{}` as `params`. It starts `EVENT` frames on this
  connection and returns the first page of current sessions. Events are
  sent only for changes after the moment of subscription; later pages
  come from `list`.

Sent

| Field  | Type    | Meaning                                                    |
| ------ | ------- | ---------------------------------------------------------- |
| `at`   | object  | `{offset, marker}`: the session's cursor once the PTY took the bytes |
| `wait` | Waited? | With `send.wait`                                           |

Waited

| Field    | Type    | Meaning                                                      |
| -------- | ------- | ------------------------------------------------------------ |
| `reason` | string  | `exited`, `matched`, `status`, `prompt`, `quiet` or `timeout` |
| `at`     | object  | `{offset, marker}`: just past the match; after the output holding the prompt; at the end of the stream; else the cursor when it ended |
| `exit`   | object? | `exited`: the session's `exit`                               |
| `match`  | object? | `matched`: `{text, offset}`, the plain text that matched and the stream offset of its first byte |
| `record` | object? | `status`: the record, as `inspect` shows it                  |

Keys

A key is modifiers joined with `+` (`ctrl`, `shift`, `alt`, `super`, any
order) and a key: `enter`, `tab`, `esc`, `backspace`, `space`, `up`,
`down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `insert`,
`delete`, `f1` to `f12`, or one character (`a`, `A` as `shift+a`, `?`,
`é`). Names ignore case but a single character's. The session's terminal
encodes each key with the engine's key encoder as a press on a US layout,
by the program's current modes (cursor and keypad application modes,
kitty keyboard flags, modifyOtherKeys), so `up` is `ESC [A` or `ESC O A`
as the program asked; all of a request's keys are encoded against the
modes when it is handled, and written as one unit.

Read

| Field    | Type    | Meaning                                                            |
| -------- | ------- | ------------------------------------------------------------------ |
| `text`   | string  |                                                                    |
| `cols`   | int     |                                                                    |
| `rows`   | int     |                                                                    |
| `cursor` | object? | `{x, y}`, zero-based; absent for a session loaded from a park file |
| `offset` | int     | Stream offset at the time of the read                              |
| `status` | string  |                                                                    |

### Session

| Field            | Type     | Meaning                                                 |
| ---------------- | -------- | ------------------------------------------------------- |
| `id`             | string   | UUID assigned by the server, never reused               |
| `label`          | string   | At most `limits.label` bytes                            |
| `status`         | string   | `running`, `exited`, `restored`                         |
| `argv`           | string[] |                                                         |
| `cwd`            | string   | Working directory at creation                           |
| `pid`            | int?     | While the child is alive                                |
| `cols`, `rows`   | int      | Current size                                            |
| `title`          | string   | From the program, OSC 0 or 2                            |
| `pwd`            | string   | From the program, OSC 7; the path only, host dropped, percent-decoded |
| `on_exit`        | string   | `remove` or `keep`                                      |
| `created_at`     | string   | RFC 3339                                                |
| `exited_at`      | string?  |                                                         |
| `exit`           | object?  | `{code}` or `{signal}`, signal by name                  |
| `offset`         | int      | Current stream offset                                   |
| `marker`         | int      | Sequence number of the latest marker, 0 if none         |
| `attachments`    | int      | Open attachments                                        |
| `program_status` | Record[] | Only in `inspect`; see Program status                   |

`restored` means the session was running when a previous server
incarnation stopped. It has a screen but no process and no exit.

`cwd`, `title`, `pwd`, and `argv` as a whole are at most `limits.text`
bytes each; longer values are refused at `create` and truncated when
reported by the program.

### Events

`EVENT` is `{event, ...}`.

| Event                    | Fields           | When                                            |
| ------------------------ | ---------------- | ----------------------------------------------- |
| `session_created`        | `session`        |                                                 |
| `session_changed`        | `session`        | Label, size, title, pwd or `attachments` changed |
| `session_exited`         | `session`        | The stream ended; `exit` is set                 |
| `session_removed`        | `id`             | Removed by request, or by `on_exit: remove`     |
| `program_status_changed` | `id, record`     | One record was set; see Program status          |
| `program_status_removed` | `id, record_ids` | Records were dropped or cleared                 |

`id` is the session id in every event that carries it.

### Program status

The server installs the engine's program-status effect, keeps one OSC 7501
record per record id, and applies the Program Status Protocol's lifetime
rules: `working` and `blocked` records are dropped when the stream ends,
at the next OSC 133 `A`, and when a session is loaded from a park file. The
root record has the id `""`. Records hold exactly the keys of their last
report; `inspect` resolves `app` from the nearest ancestor present at that
moment, as the protocol describes. At most `limits.records` records are
kept per session; beyond that the least recently updated is evicted. The
default keeps an `inspect` reply under the default `max_frame` with every
record at the protocol's maxima and worst-case JSON escaping. The server
relies on the engine's program-status and semantic-prompt effects, both on
upstream `main` as of 2026-10-18.

| Field        | Type    |
| ------------ | ------- |
| `id`         | string  |
| `state`      | string  |
| `kind`       | string? |
| `app`        | string? |
| `title`      | string? |
| `msg`        | string? |
| `progress`   | int?    |
| `updated_at` | string  |

## Attach

### Stream, offsets and markers

Each session has one stream: the PTY output bytes, plus markers. The offset
of a byte is its position in the stream, from 0 at creation. Markers are
`RESIZED` and `EXITED`. Each marker has a sequence number `seq`, from 1,
increasing per session, and the offset at which it happened. The server
retains at least the last `limits.ring` bytes of the stream and the last
`limits.markers` markers whose offsets lie within them.

Every client of a session receives the same bytes. No byte is filtered or
rewritten per client. A cursor is `{offset, marker}`: the next byte to
receive and the latest marker already reflected. A stream served from a
cursor sends the bytes from `offset` and the markers with `seq` greater
than `marker`, each marker before any byte at or past its offset.

The stream ends when the PTY master has reached end of file and the child
is reaped. The server reads the PTY to end of file before appending
`EXITED`, so no output follows it.
`exit` is the reaped child's status. A child that has exited while a
descendant keeps the PTY open leaves the session `running` with `pid`
absent. A child that closes its PTY descriptors while running keeps the
session `running` until it is reaped. Meanwhile `INPUT`, `send`,
and viewport reports are accepted and written to the PTY, where nothing
reads them.

### OPEN

| Field       | Type    | Meaning                                                  |
| ----------- | ------- | -------------------------------------------------------- |
| `session`   | string  |                                                          |
| `read_only` | bool?   | Default false                                            |
| `viewport`  | object? | `{cols, rows, cell_width?, cell_height?}`                |
| `profile`   | object? | `{fg?, bg?, cursor?, scheme?}`, see below                |
| `resume`    | object? | `{incarnation, offset, marker}`                          |

Profile colours are `#rrggbb`; `scheme` is `light` or `dark`.

Checks, in order, each failing with `ERROR` on the channel:

1. the channel is not in use, else `channel_in_use`;
2. the session exists, else `no_such_session`;
3. the client sent an `engine` in `HELLO` equal to the server's, and, for
   a session loaded from a park file, the engine recorded there equals the
   server's, else `engine_mismatch`.

A writable `OPEN` with `viewport` or `profile` on a running session is
applied as a viewport report at once (at the session's size when
`viewport` is absent), before the snapshot is taken, bypassing coalescing.
On any other session it is ignored. A `cols` or `rows` of 0, in `OPEN` or
`VIEWPORT`, is `invalid_request`, checked after the channel check and
before the session check.

### ATTACHED

| Field     | Type    | Meaning                                                 |
| --------- | ------- | ------------------------------------------------------- |
| `mode`    | string  | `snapshot` or `resume`                                  |
| `offset`  | int     | Cursor offset the attachment continues from            |
| `marker`  | int     | Cursor marker: latest `seq` reflected in the snapshot, or in the client's terminal when resuming |
| `cols`    | int     |                                                         |
| `rows`    | int     |                                                         |
| `session` | Session |                                                         |
| `snapshot_bytes` | int? | Required in mode `snapshot`, absent otherwise: the total length of the snapshot that follows (the sum of the `SNAPSHOT` payloads), so a client can size its buffer |

`resume` is chosen when `OPEN.resume.incarnation` equals the server's
incarnation, `OPEN.resume.offset` lies within the retained stream, and
every marker with `seq` above `OPEN.resume.marker` is retained. Otherwise
`snapshot`. Under the server's incarnation, a `marker` above the session's
latest `seq` fails with `invalid_request`; a cursor from another
incarnation is not compared.

### Bootstrap, mode `snapshot`

Server, holding the session still:

1. record the cursor `{cut, marker}` from the current offset and latest
   marker `seq`;
2. encode the terminal as a Ghostty snapshot;
3. set the attachment's cursor to it;
4. resume the session.

Then on the channel:

1. `ATTACHED {mode: "snapshot", offset: cut, marker, ...}`;
2. `SNAPSHOT` frames carrying the snapshot bytes in order, split at
   arbitrary positions;
3. `SNAPSHOT_READY` once every byte through the end of the `READY` record
   has been sent;
4. the stream from the cursor, interleaved with the remaining `SNAPSHOT`
   frames, control and stream frames taking precedence;
5. `SNAPSHOT_END` after the last snapshot byte.

No stream frame is sent before `SNAPSHOT_READY`. Input is accepted from
`ATTACHED` on. The snapshot reflects every marker up to `marker`, so a
session whose stream has already ended sends no `EXITED`; the client reads
`session.status` and `session.exit`.

Client:

1. buffer `SNAPSHOT` bytes until `SNAPSHOT_READY`, then decode through
   `READY` and show the screen;
2. feed `OUTPUT` to the terminal as it arrives;
3. feed the decoder one step at a time, only once the bytes for that step
   are buffered: a step runs from the current position through the next
   `PAGE` or `FINISH` record, inclusive. The decoder reads a whole step per
   call and is unusable after a short read;
4. on `RESIZED` before `SNAPSHOT_END`: either hold all following stream
   frames until `SNAPSHOT_END`, then resize and apply them, or stop decoding
   history. A resize applied mid-history discards the remaining history.

The client's decoder must accept continuations up to
`limits.continuation`.

### Bootstrap, mode `resume`

`ATTACHED {mode: "resume", offset, marker}` echoes the client's cursor and
is followed by the stream from it. The client keeps its terminal.

### Stream frames

`OUTPUT`: payload is `offset u64` followed by PTY bytes. `offset` is the
stream offset of the first byte.

`RESIZED`: `{seq, offset, cols, rows}`. The client resizes its terminal
when the stream reaches `offset`. A report that does not change the size
emits no marker.

`EXITED`: `{seq, offset, exit}`. Final frame of the stream. Later `INPUT`
and `VIEWPORT` fail with `not_running`.

A client tracks its cursor as it consumes frames; an `OUTPUT` whose offset
is not the expected one is a client-side error, recovered by `CLOSE` and a
new `OPEN`.

### INPUT and VIEWPORT

`INPUT` carries bytes for the PTY. The server writes each frame as one unit,
never interleaved with another frame's bytes, so a frame should hold whole
key encodings. From a read-only attachment it fails with `read_only`.

A client never sends its own terminal's output as `INPUT`: the engine's
write-to-PTY effect (`GHOSTTY_TERMINAL_OPT_WRITE_PTY`) must be left unset
or discarded on a client, or every query is answered twice. A client that
passes PTY bytes to an outer terminal does not forward that terminal's
answers to the program's queries either. A client never resizes its
terminal except at a `RESIZED` marker; a local resize desynchronises it
from the stream and discards history.

`VIEWPORT` is `{cols, rows, cell_width?, cell_height?, profile?}`, with
`profile` shaped as in `OPEN`. From a read-only attachment it fails with
`read_only`.

The server keeps at most `limits.input_queue` bytes of pending input per
session. A frame that would exceed it is dropped with `input_overflow`.

### Slow attachments

The server never slows a session for an attachment. When the stream has
advanced more than `limits.ring` bytes past an attachment's cursor, or a
marker the cursor has not received is no longer retained, the server
sends `GAP {from, to}`, meaning bytes from `from` up to `to` were not
delivered, where `to` is the `offset` of the `ATTACHED` that follows, and
begins a new bootstrap in mode `snapshot` on the same channel. The client
discards its terminal. If the bootstrap that followed a gap is still in
flight when this happens again, the server sends `ERROR too_slow`.

A client that stops reading altogether is disconnected once the server
holds more than 256 KiB plus four frames of `limits.max_frame` of
undelivered frames for it.

### CLOSE and CLOSED

`CLOSE` detaches. The server answers `CLOSED {reason: "closed"}`; the client
may reuse the channel after that. The server sends `CLOSED {reason}` on its
own when the session is removed (`removed`) or the server stops
(`stopping`). An attachment is finished when it has received the final
marker and, if bootstrapping, `SNAPSHOT_END`. An explicit `remove` sends
`CLOSED removed` to finished attachments at once and to the rest after
`limits.remove_grace`; the session stays listed as `exited` or `restored`
until then, and its files are deleted after.

`ERROR` on an attachment channel ends the attachment, except `read_only`,
`not_running` and `input_overflow`, which drop the offending frame and keep
it.

### Removal on exit

With `on_exit: remove`, the session's files are deleted when the stream
ends, and removal itself begins `limits.remove_grace` later, so a
short-lived session can still be opened or read. At that moment every
open attachment, finished or not, gets `CLOSED removed`.

## Size

A session has one size. Writable attachments report their viewport in `OPEN`
and `VIEWPORT`; `resize` is a report too. The most recent report wins. The
server coalesces `VIEWPORT` reports arriving within 25 ms, then resizes its
terminal, sets the PTY size, emits a mode 2048 report if enabled, and
appends `RESIZED` to the stream. A report applied at once discards pending
coalesced reports. Resize requests from the program (CSI 8 t) are ignored.

Since the latest report wins, a client with a person in front of it
reports only while it is in use, so the session takes the size of the
client being looked at. The web client (`mountTerminal`) reports, in
`OPEN` and `VIEWPORT`, only while its page is visible and its window has
the focus (`document.visibilityState` and `document.hasFocus()`), and
reports again as soon as it is shown or focused, whether or not its size
changed, and at its next input when another client's report resized the
session while it kept the focus. A hidden or unfocused web client sends `OPEN` without `viewport`
or `profile` and no `VIEWPORT`; its terminal follows the session's size
through `RESIZED` like any other.

A report carries a size and may carry a `profile` and a cell size; what
it omits is unchanged (`resize` carries neither). The session's profile
and cell size are those of the latest reports that carried them. They
answer the queries below and persist while no attachment is open; a
parked session has no process to answer, so they are not parked. When
the answer to the colour scheme query
changes and mode 2031 is set, the server writes the colour scheme report
to the PTY. Default colours are not part of the stream: a client that
reports a profile sets the same colours on its own terminal, as the server
does on the session's.

## Queries

The server's terminal answers every query from the PTY.

| Query                              | Answer                                             |
| ---------------------------------- | -------------------------------------------------- |
| DA1, DA2, DA3, DSR, CPR, DECRQM    | From the server's terminal                         |
| Kitty keyboard, kitty graphics     | From the server's terminal                         |
| XTVERSION                          | `clankermux <version>`                             |
| XTGETTCAP                          | Ghostty's terminfo for `xterm-ghostty`             |
| OSC 10, 11, 12                     | The session's profile; `#ffffff`, `#282c34` and `#ffffff` if unset |
| CSI ? 996 n colour scheme          | `profile.scheme`; else from `bg` when set, dark below relative luminance 0.5; else no answer |
| XTWINOPS size reports              | Session size; the session's cell size, else 0      |
| OSC 7501 `?`                       | Supported; the effect is installed                 |
| OSC 52 read                        | Not answered                                       |

OSC 52 writes are not relayed by the server: every client's engine sees the
same bytes and raises its own clipboard effect, which the client applies or
ignores locally.

## Snapshot

Snapshots are Ghostty binary snapshots (`GHOSTSNP`) as produced by the
engine named in `WELCOME.engine`, including the `CONTINUATION` record. The
server keeps continuation tracking enabled on every terminal with the limit
`limits.continuation`. If a snapshot cannot be taken because tracking is
exceeded, the session keeps running and the server retries as output
arrives, for up to 5 s, then fails the attach or park with
`limit_exceeded`. A failed park keeps the previous park files.

A client whose scrollback limit is below the server's receives truncated
history, by the decoder's own rule.

## Persisted state

The state directory is `$XDG_STATE_HOME/clankermux`, or
`~/.local/state/clankermux` when unset. The server holds an exclusive lock
on `lock` in it for its lifetime and exits if it cannot. Each parked
session is a directory `sessions/<id>/<generation>/`, with `generation`
the park time as Unix epoch milliseconds, mode 0700, files 0600:

| File                | Contents                                                     |
| ------------------- | ------------------------------------------------------------ |
| `session.json`      | `{protocol: 1, engine, parked_at, session, history_lines}`: the `inspect` object (`attachments: 0`, no `pid`, `program_status` as reported with no inherited `app`) and how many leading lines of `screen.txt` are history |
| `snapshot.ghostsnp` | The snapshot bytes                                           |
| `screen.txt`        | The `read` text with all retained history, at `parked_at`   |

A park writes the three files into a temporary directory named with a
leading dot, syncs each file and the directory, renames the directory to
its generation name, syncs `sessions/<id>/`, then deletes older
generations. On start, the newest directory holding all three files is the
session's state; the others, and any dot-named directory, are deleted.

A running session is parked:

- after a change (output, input, a resize, a label, or its creation),
  once changes have been quiet for 2 s or 30 s after the first change
  since its last park, whichever comes first, but not within 5 s of the
  start of its last park. Its files are then at most 5 s behind once
  changes stop (2 s when it has not parked in the 5 s before) and 30 s
  behind while they continue, plus the park's own time, and these parks
  of a session are at least 5 s apart; a change during a park counts
  towards the next;
- when its last attachment closes;
- every 5 minutes.

These times are the defaults; a server may be configured with others. A
park that fails is logged and tried again at any of these. A session is
also parked once at the end of the stream with `on_exit: keep`, and on
graceful stop when it is not being removed (a kept session's final park
may still be pending); a stop deletes the files of sessions being
removed, then sends SIGHUP to each running session's process group.
While the server stops, `create`, `remove` and `OPEN` fail with
`internal`.
The whole `sessions/<id>/` directory is deleted whenever the session is
removed. On start, a `session.json` whose `session.id` is not its
directory's name is skipped with a warning.

On start, each session directory becomes a session: `status` is `exited`
if `session.json` says so, `restored` otherwise. `read` serves
`screen.txt`, with `scrollback` taken from its history lines. `OPEN`
requires `session.json.engine` to equal the server's engine as well as the
client's; after an engine change only `read` and `remove` work. `OPEN`
streams `snapshot.ghostsnp` as a bootstrap with no stream frames after
`SNAPSHOT_END`.

## Carriers

Frames are carried on any ordered reliable byte stream.

| Carrier     | Rule                                                                                                              |
| ----------- | ----------------------------------------------------------------------------------------------------------------- |
| Unix socket | `$XDG_RUNTIME_DIR/clankermux/sock`, or `sock` in the state directory when unset; directory 0700, socket 0600; the server closes a connection whose peer uid differs from its own |
| TCP         | Bound only to the host's tailnet address, obtained from the Tailscale local API; plaintext. Default port 7437    |
| SSH         | `clankermux --stdio` on the remote host relays frames between its stdio and the local Unix socket                 |
| WebSocket   | TLS only; one frame per binary message, same header; bound to the tailnet address or loopback, default port 7438; the server refuses a connection whose `Origin` header is not in the configured allowlist, and every connection while the allowlist is empty |

A WebSocket upgrade is accepted on any request path; a refused `Origin`
gets HTTP 403. The TLS handshake and the upgrade count against
`limits.hello_timeout`. A binary message that is not exactly one frame, or
a text message, is a protocol error: `ERROR protocol_error` on channel 0,
then the close, as on any carrier. Configured origins are compared in the
form browsers send them (lowercase scheme and host, no default port, no
trailing slash).

Over TCP and WebSocket, trust is the tailnet policy alone: any process on
any node the policy admits, under any user on that node, can do everything
the server's user can. The `Origin` allowlist only keeps pages from other
origins, open in a browser on an admitted node, off the server. This is a
deliberate trade-off for version 1.

## Limits

Announced in `WELCOME.limits`. `control_body` bounds client-sent JSON;
server-sent JSON is bounded by `max_frame`: the defaults for `records`,
`text`, and `label` keep the largest `inspect` reply under the default
`max_frame`, and a `list` or `subscribe` page holds fewer than `page`
sessions when more would not fit (`next` still names the last one sent).

| Field          | Default    | Meaning                                       |
| -------------- | ---------- | --------------------------------------------- |
| `max_frame`    | 1 MiB      | Payload bytes per frame                       |
| `control_body` | 64 KiB     | Client JSON payload bytes                     |
| `ring`         | 4 MiB      | Retained stream bytes per session             |
| `input_queue`  | 1 MiB      | Pending input bytes per session               |
| `scrollback`   | 10 MiB     | Scrollback per session                        |
| `continuation` | 8 MiB      | Continuation tracking per session             |
| `label`        | 256        | Label bytes                                   |
| `text`         | 2048       | Bytes per `cwd`, `title`, `pwd`, and `argv`   |
| `markers`      | 1024       | Retained markers per session                  |
| `records`      | 200        | Program status records per session            |
| `page`         | 32         | Sessions per `list` or `subscribe` reply      |
| `hello_timeout`| 10000      | Milliseconds allowed for `HELLO`              |
| `remove_grace` | 10000      | Milliseconds before `on_exit: remove` removes |

## Errors

`ERROR` is `{id?, code, message}`. On channel 0 with `id` it answers a
request. On channel 0 without `id` it precedes closing the connection. On
an attachment channel it ends the attachment, except `read_only`,
`not_running` and `input_overflow`.

| Code               | Meaning                                                   |
| ------------------ | --------------------------------------------------------- |
| `protocol_error`   | A connection error under Framing; the connection closes   |
| `version_mismatch` | `HELLO.version` is not 1; the connection closes           |
| `invalid_request`  | Malformed payload, a field the protocol does not define, unknown method, or a resume marker above the session's latest |
| `limit_exceeded`   | `label` or `text` exceeded, or a snapshot failed          |
| `no_such_session`  |                                                           |
| `not_running`      | `status` is not `running`, or the stream has ended; for a wait, nothing matched before the stream ended |
| `running`          | `remove` on a running session                             |
| `signal_failed`    | `end` or `signal` found no process to signal              |
| `engine_mismatch`  | Engine ids differ                                         |
| `channel_in_use`   | `OPEN` on a channel already in use                        |
| `read_only`        | `INPUT` or `VIEWPORT` on a read-only attachment           |
| `input_overflow`   | The frame would exceed `limits.input_queue`               |
| `too_slow`         | Second gap during a bootstrap                             |
| `cancelled`        | A `cancel` ended the request                              |
| `internal`         |                                                           |

## Not in version 1

Process survival across a server restart; encryption of parked state;
authentication beyond socket ownership and tailnet policy; a WebTransport
carrier; idle parking that frees a live terminal; a
self-describing API; size policies other than latest-wins; images from
before an attach; OSC 52 reads; input acknowledgements on attachment
channels.
