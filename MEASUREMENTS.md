# Measurements

The tree of commit `ca7a99c`, Apple M4 Pro (arm64, 48 GiB), macOS, 2026-10-18, on an
otherwise idle machine (a reviewer's run under a load average above 18 measured a third
of the throughput and twice the park time). Produced by
`cargo run --release -p clankermux --example measure`, which prints the tables below.
Default limits; the server in-process; clients in the same process over loopback; one
run, wall clock. Numbers are indicative, not a benchmark.

## Idle cost

Sessions created with the example's default shell (`/bin/sh`), 80x24, nobody attached. RSS is
the measuring process's (`ps`, one sample), which holds the server, the clients and
the measurement itself and not the sessions' shells; CPU is the process's over 10 s,
in percent of one core, at the 10 ms resolution of `ps`. A single sample is noisy
at this size (another run read 4.3 MiB with one session), so the table bounds the
per-session cost rather than measuring it: fifty idle sessions add a few MiB and no
measurable CPU.

| Idle sessions | RSS | CPU over 10 s |
| --- | --- | --- |
| 0 | 4.9 MiB | |
| 1 | 5.6 MiB | 0.1% |
| 10 | 6.5 MiB | 0.0% |
| 50 | 8.7 MiB | 0.0% |

## Bootstrap

A session fed the given amount of distinct 80-column lines, then attached over
the Unix socket: the snapshot's size (the sum of the `SNAPSHOT` payloads) and
the time from `OPEN` to `SNAPSHOT_END`.

| Scrollback written | Rows retained | Snapshot bytes | OPEN to SNAPSHOT_END |
| --- | --- | --- | --- |
| 0 MiB | 0 | 0.00 MiB | 0 ms |
| 1 MiB | 13086 | 1.03 MiB | 1 ms |
| 4 MiB | 14583 | 1.14 MiB | 1 ms |
| 10 MiB | 14624 | 1.15 MiB | 1 ms |

`limits.scrollback` (10 MiB by default) bounds the engine's page memory, not the
text: 10 MiB of pages hold about 14,600 rows of 80 cells, about 1.1 MiB of text,
so the snapshot stops growing there. The bootstrap is a memory copy; it takes one
to two milliseconds at this size.

## Park

A session with 10 MiB of scrollback written (the retained 14,624 rows) parks in
20 ms into 2.27 MiB of files (`snapshot.ghostsnp`, `screen.txt`, `session.json`).

## Stream throughput

64 MiB of distinct 80-column lines from the PTY (`awk | head -c`) to one
attached client, from the first `OUTPUT` to the last; `Gaps` counts `GAP`
frames (none: the client kept up).

| Carrier | MiB/s | Gaps |
| --- | --- | --- |
| unix | 35 | 0 |
| tcp | 34 | 0 |
| wss | 34 | 0 |

The three carriers are within noise of each other, so the WebSocket's TLS and
framing cost nothing visible at this rate. The `wss` row drives the Rust client
through the server's own WebSocket adapter (the Rust client has no WebSocket
endpoint); it measures the carrier, not the browser client. The bound is upstream of the carrier:
the `awk` pipeline alone writes 64 MiB to `/dev/null` at about 105 MiB/s, so it
is the PTY, the engine's parse or the stream path, which this run does not
separate.
