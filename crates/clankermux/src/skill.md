# clankermux

clankermux runs terminal sessions on a server. A session is one program in
its own terminal (a PTY): it has an id (a UUID, never reused), a label you
may set, a size, a screen with history, and a status: `running`, `exited`
(with its exit code or signal), or `restored` (it was running when the
server last stopped; its screen is kept, its process is gone). Sessions
stay when you detach. People and agents can attach to the same session.

## Where you are

A program started by clankermux has two variables in its environment:

- `CLANKERMUX_SESSION`: the id of the session it runs in.
- `CLANKERMUX_SOCKET`: the server's socket.

When they are set, `clankermux` commands talk to that server, and a
command that takes a session acts on this one when you leave it out. When
they are not set, you are not inside a session; name the session.

## Naming a session

A session argument is the full id; else a label, ignoring case; else four
or more characters from the start or the end of the id. `list` shows the
last six characters of each id. When an argument names no session or
several, the candidates are printed to stderr and the exit code is 3: pick
one by id. To keep an id, parse it from `--json` output rather than from
the human-readable listing.

## Commands

    clankermux list                          # every session, one line each, with what needs attention
    clankermux --json list                   # the same as JSON
    clankermux create --label build -- make  # start a session; prints its id
    clankermux create --keep -- ./test.sh    # keep it listed after it exits
    clankermux create --wait -- make test    # run it, wait, exit with its status
    clankermux --json create --wait -- make test   # the same; prints {session, wait}
    clankermux inspect build                 # one session as JSON: program status, attachments
    clankermux read build --scrollback 200   # its screen, with 200 history lines before it
    clankermux read build --scope last_command   # the last command's output (needs shell integration)
    clankermux read build --format html      # the screen as styled HTML (or vt)
    clankermux tail build --follow           # its output as plain text, as it comes
    clankermux send build 'make test' --enter   # type text, then Enter
    clankermux send build --keys ctrl+c         # keys by name: enter, tab, esc, up, f5, shift+tab, alt+x
    clankermux send build --paste "$(cat notes.txt)"  # a paste, as if pasted by a person
    clankermux send build --bytes < input.bin   # send stdin's bytes as they are
    clankermux send build 'make' --enter --wait-match 'error|done' --timeout 5m
                                             # send, then wait for what it prints
    clankermux wait build --exit             # wait for it to exit
    clankermux wait build --prompt           # wait for the shell's next prompt (OSC 133)
    clankermux wait build --status done,error   # wait for a program status (OSC 7501)
    clankermux wait build --quiet 2000       # wait until it prints nothing for 2 s
    clankermux label build release           # set the label
    clankermux resize build 120x40           # set the size (or 120x, x40)
    clankermux resize build --policy fixed   # only resize sets the size (or latest, smallest)
    clankermux detach build 42 --reason why  # close attachment 42, by its channel in inspect
    clankermux signal build INT              # send a signal by name
    clankermux end build                     # hang up its processes
    clankermux remove build                  # delete an exited session
    clankermux ack build                     # mark its done and error reports seen
    clankermux events                        # session events as JSON lines, each with its seq
    clankermux events --after 41 --incarnation ID   # the events after seq 41, then the live ones
    clankermux attach build                  # attach this terminal (people, not agents)

Inside a session, leave the session out: `clankermux label building`.
`events` names the server's incarnation on stderr; `seq` counts from 1
again when the server restarts, so keep both to resume.

A wait given with `send` starts before the text is written, so it cannot
miss a quick answer; its `--status` counts only a status set after the
send. `--match` sees the output as plain text, without colours or other
escape sequences, and that includes the terminal's echo of what you
typed: match on what only the program prints (`--match 'error|done'`
also matches the echo of `echo done`). Ctrl-C cancels a wait.

`send` with one argument types it into the session you are in; when that
argument names a session, it refuses: give both SESSION and TEXT. Outside
a session, or with `--connect` or `--ssh`, name the session.

## Program status

A program reports its status with OSC 7501: `working`, `blocked` (it
needs you), `done`, `error` or `idle`, for itself or for named parts of
its work. `inspect` shows the reports. `done` and `error` reports carry
`seen`, false until `ack` marks them seen. A session's `attention` is its
most urgent report that wants someone: `blocked`, then an unseen `error`,
then an unseen `done`, then `working`. `list` prints it after the status.
Ack what you have dealt with; leave others' reports alone unless asked.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Done |
| 1 | Failed: the server refused the request, or the connection broke |
| 2 | Usage: bad arguments, or no session named outside a session |
| 3 | The session argument named no session, or several |
| 4 | The server cannot be reached |
| 124 | A wait timed out |
| 130 | A wait was cancelled with Ctrl-C |

A wait that ends because the session exited (`--exit`, `create --wait`)
exits with the program's status instead: its exit code, or 128 plus the
signal's number.

## Rules

- Do not end, signal or remove a session you did not create, and do not
  detach anyone or change a session's size policy, unless you are asked
  to.
- Never send input to a session you did not create unless you are asked
  to: someone may be typing in it.
- Keep the ids of the sessions you create, from `--json` output, and act
  on those ids; labels can repeat and can change.
- `read` before you act on what a session shows; its screen changes while
  you wait.
- Remove the sessions you created with `--keep` once you are done with
  them.
