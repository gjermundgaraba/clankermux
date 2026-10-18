# clankermux protocol, version 1

A terminal multiplexer daemon with flat sessions, one PTY each. Clients and
server embed the same pinned libghostty-vt build. A client attaches by
decoding a Ghostty binary snapshot, then feeds raw PTY bytes to its own
terminal. The server is the only writer to a PTY.

## Conventions

- Integers in frame headers and raw payloads are unsigned little-endian.
- JSON payloads are UTF-8 objects. Unknown fields are ignored.
- `base64` fields are standard base64 with padding.
- Byte sizes are in bytes. `MiB` is 1,048,576 bytes.
- The engine id is an opaque string naming the libghostty-vt build, compared
  for exact equality. Recommended form: `ghostty-vt@<upstream commit>`.

## Framing

Every frame is a 10-byte header followed by `length` payload bytes.

| Offset | Size | Field     | Meaning                                         |
| ------ | ---- | --------- | ----------------------------------------------- |
| 0      | 1    | `type`    | Frame type, see below                           |
| 1      | 1    | `flags`   | Reserved, send 0                                |
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
| `0x98` | server    | n       | JSON      | `CLIPBOARD`      |
| `0x99` | server    | n       | JSON      | `CLOSED`         |

Channels `1` to `2^32-1` are chosen by the client, one per attachment. A
channel is in use from `OPEN` until the server's `CLOSED`, or an `ERROR`
that ends the attachment (see Errors).

A frame with an unknown type or a length above `limits.max_frame` is a
connection error: the server sends `ERROR` on channel 0 and closes the
connection. A frame other than `OPEN` on a channel that is not in use is
dropped. Nonzero `flags` are ignored.

## Handshake

The client sends `HELLO` first. The server answers `WELCOME`, or `ERROR
version_mismatch` and then closes the connection. Before `WELCOME`, the
default limits apply.

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
or `ERROR {id, code, message}`. `id` is a client-chosen integer. Requests
on a connection are processed in order.

| Method      | Params                                                      | Result                 |
| ----------- | ----------------------------------------------------------- | ---------------------- |
| `create`    | `label?, argv?, cwd?, env?, cols?, rows?, on_exit?`         | Session                |
| `list`      |                                                             | `{sessions: Session[]}`|
| `inspect`   | `session`                                                   | Session                |
| `end`       | `session`                                                   | `{}`                   |
| `signal`    | `session, signal`                                           | `{}`                   |
| `remove`    | `session`                                                   | `{}`                   |
| `set_label` | `session, label`                                            | Session                |
| `set_size`  | `session, cols, rows`                                       | `{}`                   |
| `read`      | `session, scrollback?`                                      | Read                   |
| `write`     | `session, data`                                             | `{}`                   |
| `subscribe` |                                                             | `{}`                   |

- `create`: `argv` defaults to the user's login shell, `cwd` to the server
  user's home directory, `cols` to 80, `rows` to 24, `on_exit` to `remove`. `env`
  is an object of additions and overrides; the server sets `TERM` to
  `xterm-ghostty` unless overridden. The child runs in a new session with
  the PTY as its controlling terminal.
- `end` sends SIGHUP to the child's process group. `signal` sends the named
  signal (`HUP`, `INT`, `TERM`, `KILL`, ...). Both reply at once; the exit
  arrives as an event. Both fail with `not_running` unless `status` is
  `running`.
- `remove` deletes a session whose `status` is `exited` or `restored`,
  including its files. Fails with `running` otherwise.
- `set_size` counts as a viewport report (see Size).
- `read`: `scrollback` is a line count, default 0. `text` is the last
  `scrollback` history lines followed by the screen, one line per row,
  trailing blanks trimmed, soft wraps joined. Works for every status.
- `write`: `data` is base64. Replies after the bytes are written to the PTY
  as one unit. Fails with `not_running` unless `status` is `running`.
- `subscribe` starts `EVENT` frames on this connection.

Read

| Field    | Type    | Meaning                                  |
| -------- | ------- | ---------------------------------------- |
| `text`   | string  |                                          |
| `cols`   | int     |                                          |
| `rows`   | int     |                                          |
| `cursor` | object? | `{x, y}`, zero-based; absent for a session loaded from a park file |
| `offset` | int     | Stream offset at the time of the read    |
| `status` | string  |                                          |

### Session

| Field            | Type     | Meaning                                              |
| ---------------- | -------- | ---------------------------------------------------- |
| `id`             | string   | UUID assigned by the server, never reused            |
| `label`          | string   | At most `limits.label` bytes                         |
| `status`         | string   | `running`, `exited`, `restored`                      |
| `argv`           | string[] |                                                      |
| `cwd`            | string   | Working directory at creation                        |
| `pid`            | int?     | While running                                        |
| `cols`, `rows`   | int      | Current size                                         |
| `title`          | string   | From the program, OSC 0 or 2                         |
| `pwd`            | string   | From the program, OSC 7                              |
| `on_exit`        | string   | `remove` or `keep`                                   |
| `created_at`     | string   | RFC 3339                                             |
| `exited_at`      | string?  |                                                      |
| `exit`           | object?  | `{code}` or `{signal}`                               |
| `offset`         | int      | Current stream offset                                |
| `attachments`    | int      | Open attachments                                     |
| `program_status` | Record[] | OSC 7501 records, see Program status                 |

`restored` means the session was running when a previous server
incarnation stopped. It has a screen but no process and no exit.

### Events

`EVENT` is `{event, ...}`.

| Event             | Fields    | When                                                  |
| ----------------- | --------- | ----------------------------------------------------- |
| `session_created` | `session` |                                                       |
| `session_changed` | `session` | Label, size, title, pwd, program status, or status    |
| `session_exited`  | `session` | The child exited; `exit` is set                       |
| `session_removed` | `id`      | Removed by request, or by `on_exit: remove`           |

With `on_exit: remove`, `session_exited` is followed by `session_removed`.

### Program status

The server keeps the OSC 7501 records of each session as the Program Status
Protocol specifies, one per `id`, with its lifetime rules.

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

### Stream and offsets

Each session has one stream: the PTY output bytes with `RESIZED` and
`EXITED` markers at the positions where they happened. The offset of a byte
is its position in the stream, from 0 at creation. The server retains at
least the last `limits.ring` bytes of the stream.

Every client of a session receives the same bytes. No byte is filtered or
rewritten per client.

A marker at offset `X` comes after every byte before `X` and before any
byte at `X`. A stream served from `X` includes the markers at `X`.

### OPEN

| Field       | Type    | Meaning                                                  |
| ----------- | ------- | -------------------------------------------------------- |
| `session`   | string  |                                                          |
| `read_only` | bool?   | Default false                                            |
| `viewport`  | object? | `{cols, rows, cell_width?, cell_height?}`                |
| `profile`   | object? | `{fg?, bg?, cursor?}`, colours as `#rrggbb`              |
| `resume`    | object? | `{incarnation, offset}`                                  |

Checks, in order, each failing with `ERROR` on the channel:

1. the channel is not in use, else `channel_in_use`;
2. the session exists, else `no_such_session`;
3. the client sent an `engine` in `HELLO` equal to the server's, or, for a
   session loaded from a park file, equal to the engine recorded there,
   else `engine_mismatch`.

A writable `OPEN` with `viewport` on a running session counts as a
viewport report before the snapshot is taken. On any other session it is
ignored.

### ATTACHED

| Field     | Type    | Meaning                                       |
| --------- | ------- | --------------------------------------------- |
| `mode`    | string  | `snapshot` or `resume`                        |
| `offset`  | int     | Stream offset the attachment continues from   |
| `cols`    | int     |                                               |
| `rows`    | int     |                                               |
| `session` | Session |                                               |

`resume` is chosen when `OPEN.resume.incarnation` equals the server's
incarnation and `OPEN.resume.offset` lies within the retained stream.
Otherwise `snapshot`.

### Bootstrap, mode `snapshot`

Server, holding the session still:

1. record `cut`, the current stream offset;
2. encode the terminal as a Ghostty snapshot;
3. set the attachment's cursor to `cut`;
4. resume the session.

Then on the channel:

1. `ATTACHED {mode: "snapshot", offset: cut, ...}`;
2. `SNAPSHOT` frames carrying the snapshot bytes in order, split at
   arbitrary positions;
3. `SNAPSHOT_READY` once every byte through the end of the `READY` record
   has been sent;
4. `OUTPUT`, `RESIZED`, `EXITED` from `cut`, interleaved with the remaining
   `SNAPSHOT` frames, control and output taking precedence;
5. `SNAPSHOT_END` after the last snapshot byte.

No `OUTPUT`, `RESIZED` or `EXITED` is sent before `SNAPSHOT_READY`. Input is
accepted from `ATTACHED` on.

Client:

1. buffer `SNAPSHOT` bytes until `SNAPSHOT_READY`, then decode through
   `READY` and show the screen;
2. feed `OUTPUT` to the terminal as it arrives;
3. split later `SNAPSHOT` bytes into records at the 10-byte record header
   and feed the decoder only complete records;
4. on `RESIZED` before `SNAPSHOT_END`: either hold all following stream
   frames until `SNAPSHOT_END`, then resize and apply them, or stop decoding
   history. A resize applied mid-history discards the remaining history.

### Bootstrap, mode `resume`

`ATTACHED {mode: "resume", offset}` followed by the stream from `offset`.
The client keeps its terminal.

### Stream frames

`OUTPUT`: payload is `offset u64` followed by PTY bytes. `offset` is the
stream offset of the first byte.

`RESIZED`: `{offset, cols, rows}`. The client resizes its terminal when the
stream reaches `offset`.

`EXITED`: `{offset, exit}`. Final frame of the stream. Later `INPUT` and
`VIEWPORT` fail with `not_running`.

A client tracks `offset` as the next expected position; a mismatch is a
client-side error, recovered by `CLOSE` and a new `OPEN`.

### INPUT and VIEWPORT

`INPUT` carries bytes for the PTY. The server writes each frame as one unit,
never interleaved with another frame's bytes, so a frame should hold whole
key encodings. From a read-only attachment it fails with `read_only`.

`VIEWPORT` is `{cols, rows, cell_width?, cell_height?, profile?}`. From a
read-only attachment it fails with `read_only`.

The server keeps at most `limits.input_queue` bytes of pending input per
session. Input beyond it fails with `input_overflow`.

### Slow attachments

The server never slows a session for an attachment. When the stream has
advanced more than `limits.ring` bytes past an attachment's cursor, the
server sends `GAP {from, to}`, meaning bytes from `from` up to `to` were not
delivered, and begins a new bootstrap in mode `snapshot` on the same
channel. The client discards its terminal. If a bootstrap is
still in flight when this happens again, the server sends `ERROR too_slow`.

### CLOSE and CLOSED

`CLOSE` detaches. The server answers `CLOSED {reason: "closed"}`; the client
may reuse the channel after that. The server sends `CLOSED {reason}` on its
own when the session is removed (`removed`) or the server stops
(`stopping`).

`ERROR` on an attachment channel ends the attachment, except `read_only`
and `not_running`, which drop the offending frame and keep it.

### CLIPBOARD

`{data}`: base64 of what the program wrote with OSC 52. Sent to every
attachment. Each client decides whether to apply it. OSC 52 reads are not
answered.

## Size

A session has one size. Writable attachments report their viewport in `OPEN`
and `VIEWPORT`; `set_size` is a report too. The most recent report wins. The
server coalesces reports arriving within 25 ms, then resizes its terminal,
sets the PTY size, emits a mode 2048 report if enabled, and appends
`RESIZED` to the stream.

The authoritative attachment is the one whose report was adopted last;
`set_size` changes the size but not the authoritative attachment. Its
`profile` and cell size answer the queries below, persist while no
attachment is open, and are parked with the session.

## Queries

The server's terminal answers every query from the PTY. Clients never write
to the PTY on their own.

| Query                              | Answer                                           |
| ---------------------------------- | ------------------------------------------------ |
| DA1, DA2, DA3, DSR, CPR, DECRQM    | From the server's terminal                       |
| Kitty keyboard, kitty graphics     | From the server's terminal                       |
| XTVERSION                          | `clankermux <version>`                           |
| XTGETTCAP                          | Ghostty's terminfo for `xterm-ghostty`           |
| OSC 10, 11, 12                     | The authoritative profile; defaults if unset     |
| XTWINOPS size reports              | Session size; authoritative cell size, else 0    |
| OSC 52 read                        | Not answered                                     |

## Snapshot

Snapshots are Ghostty binary snapshots (`GHOSTSNP`) as produced by the
engine named in `WELCOME.engine`, including the `CONTINUATION` record. The
server keeps continuation tracking enabled on every terminal with the limit
`limits.continuation`. If a snapshot cannot be taken because tracking is
exceeded, the server waits for the parser to return to ground, up to 5 s,
then fails the attach with `limit_exceeded`.

A client whose scrollback limit is below the server's receives truncated
history, by the decoder's own rule.

## Persisted state

The state directory is `$XDG_STATE_HOME/clankermux`, or
`~/.local/state/clankermux` when unset. Each parked session is a directory
`sessions/<id>/` in it, mode 0700, files 0600:

| File                | Contents                                                       |
| ------------------- | -------------------------------------------------------------- |
| `session.json`      | `{protocol: 1, engine, parked_at, session, profile, cell}`     |
| `snapshot.ghostsnp` | The snapshot bytes                                             |
| `screen.txt`        | The `read` text of the screen at `parked_at`                   |

Written for every running session every 5 minutes and on graceful stop,
after which the server sends SIGHUP to each running session's process
group, and once at exit for sessions with `on_exit: keep`. Deleted whenever
the session is removed.

On start, each directory becomes a session: `status` is `exited` if
`session.json` says so, `restored` otherwise. `read` serves `screen.txt`.
`OPEN` requires the client's engine to equal `session.json.engine` and
streams `snapshot.ghostsnp` as a bootstrap with no stream frames after
`SNAPSHOT_END`.

## Carriers

Frames are carried on any ordered reliable byte stream.

| Carrier     | Rule                                                                                                              |
| ----------- | ----------------------------------------------------------------------------------------------------------------- |
| Unix socket | `$XDG_RUNTIME_DIR/clankermux/sock`, or `sock` in the state directory when unset; directory 0700, socket 0600; the server closes a connection whose peer uid differs from its own |
| TCP         | Bound only to the host's tailnet address; plaintext; trust is the tailnet policy. Default port 7437              |
| SSH         | `clankermux --stdio` on the remote host relays frames between its stdio and the local Unix socket                 |

## Limits

Announced in `WELCOME.limits`.

| Field          | Default    | Meaning                                       |
| -------------- | ---------- | --------------------------------------------- |
| `max_frame`    | 1 MiB      | Payload bytes per frame                       |
| `control_body` | 64 KiB     | JSON payload bytes                            |
| `ring`         | 4 MiB      | Retained stream bytes per session             |
| `input_queue`  | 1 MiB      | Pending input bytes per session               |
| `scrollback`   | 10 MiB     | Scrollback per session                        |
| `continuation` | 8 MiB      | Continuation tracking per session             |
| `label`        | 256        | Label bytes                                   |

## Errors

`ERROR` is `{id?, code, message}`. On channel 0 with `id` it answers a
request. On channel 0 without `id` it precedes closing the connection. On
an attachment channel it ends the attachment, except `read_only` and
`not_running`.

| Code               | Meaning                                                   |
| ------------------ | --------------------------------------------------------- |
| `version_mismatch` | `HELLO.version` is not 1; the connection closes           |
| `invalid_request`  | Malformed payload or unknown method                       |
| `limit_exceeded`   | A limit in Limits was exceeded, or a snapshot failed      |
| `no_such_session`  |                                                           |
| `not_running`      | `status` is not `running`, or the stream has ended        |
| `running`          | `remove` on a running session                             |
| `engine_mismatch`  | Engine ids differ                                         |
| `channel_in_use`   | `OPEN` on a channel already in use                        |
| `read_only`        | `INPUT` or `VIEWPORT` on a read-only attachment           |
| `input_overflow`   | Pending input exceeded `limits.input_queue`               |
| `too_slow`         | Second gap during a bootstrap                             |
| `internal`         |                                                           |

## Not in version 1

Process survival across a server restart; encryption of parked state;
a browser client or WebSocket carrier; idle parking that frees a live
terminal; a self-describing API; size policies other than latest-wins;
images from before an attach; OSC 52 reads; input acknowledgements on
attachment channels.
