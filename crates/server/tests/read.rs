//! `read` formats and scopes, command marks, and `tail`.

mod common;

use common::*;
use proto::*;
use serde_json::{Value, json};

async fn read_with(c: &mut Client, params: Value) -> Read {
    serde_json::from_value(c.ok("read", params).await).unwrap()
}

async fn tail(c: &mut Client, params: Value) -> Tail {
    serde_json::from_value(c.ok("tail", params).await).unwrap()
}

/// The `command` of each `session_changed` for `id` received so far.
fn changed_commands(c: &mut Client, id: &str) -> Vec<Command> {
    std::mem::take(&mut c.events)
        .into_iter()
        .filter_map(|event| match event.event {
            Event::SessionChanged { session } if session.id == id => session.command,
            _ => None,
        })
        .collect()
}

/// The offset just past the first `needle` in `stream`.
fn past(stream: &[u8], needle: &[u8]) -> u64 {
    let at = stream
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap();
    (at + needle.len()) as u64
}

#[tokio::test]
async fn each_format_on_a_known_screen() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "printf 'plain \\033[31mred\\033[0m <&>\\n'; exec sleep 5"], "cols": 20, "rows": 5}))
        .await;
    let text = wait_text(&mut c, &session.id, "red").await;
    assert_eq!(text.text, "plain red <&>\n");
    assert!(!text.truncated);
    let vt = read_with(&mut c, json!({"session": session.id, "format": "vt"})).await;
    assert!(
        vt.text.contains("plain \x1b[0m\x1b[38;5;1mred\x1b[0m <&>"),
        "{:?}",
        vt.text
    );
    let html = read_with(&mut c, json!({"session": session.id, "format": "html"})).await;
    assert_eq!(
        html.text,
        "<div style=\"font-family: monospace; white-space: pre;\">plain <div style=\"display: \
         inline;color: var(--vt-palette-1);\">red</div> &lt;&amp;&gt;</div>"
    );
    // The rest of the reply is as for text.
    assert_eq!((html.cols, html.rows, html.cursor), (20, 5, text.cursor));
    assert_eq!(
        c.err("read", json!({"session": session.id, "format": "pdf"}))
            .await,
        ErrorCode::InvalidRequest
    );
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn each_scope() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=1; while [ $i -le 30 ]; do echo line$i; i=$((i+1)); done; exec sleep 5"], "cols": 20, "rows": 5}))
        .await;
    wait_text(&mut c, &session.id, "line30").await;
    let all: String = (1..=30).map(|i| format!("line{i}\n")).collect();
    let history = read_with(&mut c, json!({"session": session.id, "scope": "history"})).await;
    assert_eq!(history.text, all);
    let html = read_with(
        &mut c,
        json!({"session": session.id, "scope": "history", "format": "html"}),
    )
    .await;
    assert!(html.text.contains(all.trim_end()), "{:?}", html.text);
    // The screen holds line27 to line30; `scrollback` counts lines before
    // it in every format.
    let screen = read_with(&mut c, json!({"session": session.id, "scrollback": 2})).await;
    assert_eq!(
        screen.text,
        "line25\nline26\nline27\nline28\nline29\nline30\n"
    );
    let vt = read_with(
        &mut c,
        json!({"session": session.id, "scrollback": 2, "format": "vt"}),
    )
    .await;
    assert!(vt.text.starts_with("line25\r\nline26\r\n"), "{:?}", vt.text);
    assert!(vt.text.contains("line30"));
    assert_eq!(
        c.err(
            "read",
            json!({"session": session.id, "scope": "history", "scrollback": 2})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        c.err("read", json!({"session": session.id, "scope": "alternate"}))
            .await,
        ErrorCode::ScopeUnavailable
    );
    // No shell integration, no command.
    assert_eq!(c.inspect(&session.id).await.command, None);
    assert_eq!(
        c.err(
            "read",
            json!({"session": session.id, "scope": "last_command"})
        )
        .await,
        ErrorCode::ScopeUnavailable
    );
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;

    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo primary; printf '\\033[?1049h\\033[31mon alt'; exec sleep 5"], "cols": 20, "rows": 5}))
        .await;
    wait_text(&mut c, &session.id, "on alt").await;
    let alternate = read_with(&mut c, json!({"session": session.id, "scope": "alternate"})).await;
    assert_eq!(
        alternate.text, "\non alt\n",
        "the cursor was on the second row"
    );
    let html = read_with(
        &mut c,
        json!({"session": session.id, "scope": "alternate", "format": "html"}),
    )
    .await;
    assert!(html.text.contains("var(--vt-palette-1);\">on alt</div>"));
    // The screen is the active one; the primary is out of reach.
    let history = read_with(&mut c, json!({"session": session.id, "scope": "history"})).await;
    assert_eq!(history.text, "\non alt\n");
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

/// What the scripted shell in `marks_from_a_scripted_shell` writes, as
/// the PTY passes it on (`\n` becomes `\r\n`), before and after it reads
/// a line.
const FIRST: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out1\r\n";
const SECOND: &[u8] = b"out2\r\n\x1b]133;D;3\x07\x1b]133;A\x07$ ";
const SCRIPT: &str = "stty -echo; printf '\\033]133;A\\007$ \\033]133;B\\007ls\\n\\033]133;C\\007out1\\n'; read x; printf 'out2\\n\\033]133;D;3\\007\\033]133;A\\007$ '; exec cat";

#[tokio::test]
async fn marks_from_a_scripted_shell() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", SCRIPT], "cols": 20, "rows": 5}))
        .await;
    let first = Command {
        start: past(FIRST, b"133;A\x07"),
        output: Some(past(FIRST, b"133;C\x07")),
        end: None,
        exit: None,
    };
    // The command runs: its output so far.
    let running = inspect_until(&mut c, &session.id, |s| s.command == Some(first)).await;
    assert_eq!(running.offset, FIRST.len() as u64);
    let read = read_with(
        &mut c,
        json!({"session": session.id, "scope": "last_command"}),
    )
    .await;
    assert_eq!(read.text, "out1\n");

    c.ok("send", json!({"session": session.id, "data": b64("\r")}))
        .await;
    let ended = Command {
        end: Some(FIRST.len() as u64 + past(SECOND, b"133;D;3\x07")),
        exit: Some(3),
        ..first
    };
    // At the next prompt, `command` is still the one that ran.
    inspect_until(&mut c, &session.id, |s| {
        s.offset == (FIRST.len() + SECOND.len()) as u64
    })
    .await;
    assert_eq!(c.inspect(&session.id).await.command, Some(ended));
    // `last_command` is the newest command with output, from C to D.
    for (format, expected) in [("text", "out1\nout2\n"), ("vt", "out1\r\nout2")] {
        let read = read_with(
            &mut c,
            json!({"session": session.id, "scope": "last_command", "format": format}),
        )
        .await;
        assert_eq!(read.text, expected, "{format}");
        assert!(!read.truncated);
    }
    // One `session_changed` per change of `command`: at C and at D.
    assert_eq!(changed_commands(&mut c, &session.id), [first, ended]);
    // The stream from the output mark, as plain text.
    let page = tail(
        &mut c,
        json!({"session": session.id, "after": first.output.unwrap()}),
    )
    .await;
    assert_eq!(page.text, "out1\nout2\n$ ");
    assert!(page.complete && !page.truncated);
    assert_eq!(page.next_offset, (FIRST.len() + SECOND.len()) as u64);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn replies_cut_to_max_frame_are_truncated() {
    let limits = Limits {
        max_frame: 8192,
        ring: 4096,
        ..fast_limits()
    };
    let budget = 8192 - READ_MARGIN;
    let server = start(limits);
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=1; while [ $i -le 1000 ]; do printf '\\033[1mline%d\\033[0m\\n' $i; i=$((i+1)); done; exec sleep 5"], "cols": 20, "rows": 5}))
        .await;
    wait_text(&mut c, &session.id, "line1000").await;
    for format in ["text", "vt", "html"] {
        let read = read_with(
            &mut c,
            json!({"session": session.id, "scope": "history", "format": format}),
        )
        .await;
        assert!(read.truncated, "{format}");
        assert!(read.text.len() <= budget, "{format}: {}", read.text.len());
        // Cut no further than needed: one more line would not fit. The
        // budget is the text's length as a JSON string.
        let json = serde_json::to_string(&read.text).unwrap().len() - 2;
        assert!(json <= budget && json + 100 > budget, "{format}: {json}");
        // The newest lines are kept, the oldest gone.
        assert!(read.text.contains("line1000"), "{format}");
        assert!(!read.text.contains("line1\n"), "{format}");
    }
    let html = read_with(
        &mut c,
        json!({"session": session.id, "scope": "history", "format": "html"}),
    )
    .await;
    assert!(html.text.starts_with("<div") && html.text.ends_with("</div>"));
    let fits = read_with(&mut c, json!({"session": session.id})).await;
    assert!(!fits.truncated);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;

    // A command whose output start the ring no longer holds.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "printf '\\033]133;C\\007'; i=1; while [ $i -le 1000 ]; do echo out$i; i=$((i+1)); done; printf '\\033]133;D;0\\007'; exec sleep 5"], "cols": 20, "rows": 5}))
        .await;
    inspect_until(&mut c, &session.id, |s| {
        s.command.is_some_and(|c| c.end.is_some())
    })
    .await;
    let read = read_with(
        &mut c,
        json!({"session": session.id, "scope": "last_command"}),
    )
    .await;
    assert!(read.truncated);
    assert!(read.text.ends_with("out999\nout1000\n"), "{:?}", read.text);
    assert!(!read.text.contains("out1\n"));
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn tail_pages_across_the_ring_boundary() {
    let server = start(Limits {
        ring: 1024,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=1; while [ $i -le 300 ]; do printf '\\033[2mrow%04d\\033[m\\n' $i; i=$((i+1)); done; exec sleep 5"], "cols": 20, "rows": 5}))
        .await;
    let end = inspect_until(&mut c, &session.id, |s| s.offset == 300 * 16)
        .await
        .offset;
    let whole = tail(&mut c, json!({"session": session.id, "after": 0})).await;
    assert!(whole.truncated && whole.complete);
    assert_eq!((whole.start_offset, whole.next_offset), (end - 1024, end));
    assert!(
        whole.text.ends_with("row0299\nrow0300\n"),
        "{:?}",
        whole.text
    );
    // 1024 bytes are 64 rows of 16: the cut falls on a row's start.
    assert!(whole.text.starts_with("row0237\n"), "{:?}", whole.text);

    // Small pages add up to the same text; each ends on a boundary.
    let mut after = whole.start_offset;
    let mut text = String::new();
    loop {
        let page = tail(
            &mut c,
            json!({"session": session.id, "after": after, "max_bytes": 100}),
        )
        .await;
        assert!(!page.truncated);
        assert!(page.next_offset - page.start_offset <= 100 + 15);
        text.push_str(&page.text);
        after = page.next_offset;
        if page.complete {
            break;
        }
    }
    assert_eq!(text, whole.text);

    assert_eq!(
        c.err("tail", json!({"session": session.id, "after": end + 1}))
            .await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        c.err(
            "tail",
            json!({"session": session.id, "after": 0, "max_bytes": 0})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    let at_end = tail(&mut c, json!({"session": session.id, "after": end})).await;
    assert_eq!(at_end.text, "");
    assert!(at_end.complete);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exited_and_restored_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({
            "on_exit": "keep",
            "argv": ["/bin/sh", "-c", "printf '\\033]133;A\\007$ \\033]133;C\\007kept output\\n\\033]133;D;0\\007'; exit 2"],
        }))
        .await;
    let exited = c.wait_exit(&session.id).await;
    let command = exited.command.expect("marked");
    assert_eq!(command.exit, Some(0));
    // An exited session keeps its stream.
    let page = tail(&mut c, json!({"session": session.id, "after": 0})).await;
    assert_eq!(page.text, "$ kept output\n");
    assert!(page.complete && !page.truncated);
    let last = read_with(
        &mut c,
        json!({"session": session.id, "scope": "last_command"}),
    )
    .await;
    assert_eq!(last.text, "kept output\n");
    let vt = read_with(&mut c, json!({"session": session.id, "format": "vt"})).await;
    a.registry.stop().await;
    drop(c);
    drop(a);

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    let restored = c.inspect(&session.id).await;
    assert_eq!(restored.command, Some(command), "marks are parked");
    // A park keeps no stream bytes.
    let page = tail(&mut c, json!({"session": session.id, "after": 0})).await;
    assert_eq!(page.text, "");
    assert_eq!(
        (page.start_offset, page.next_offset),
        (exited.offset, exited.offset)
    );
    assert!(page.complete && page.truncated);
    let last = read_with(
        &mut c,
        json!({"session": session.id, "scope": "last_command"}),
    )
    .await;
    assert_eq!(last.text, "");
    assert!(last.truncated);
    // VT and HTML come from the parked snapshot.
    let parked = read_with(&mut c, json!({"session": session.id, "format": "vt"})).await;
    assert_eq!(parked.text, vt.text);
    assert_eq!(parked.status, Status::Exited);
    assert_eq!(
        c.err("read", json!({"session": session.id, "scope": "alternate"}))
            .await,
        ErrorCode::ScopeUnavailable
    );
}

/// What Ghostty's bash integration writes at a prompt: the prompt start
/// with redraw options, the primary prompt's own mark, a secondary
/// prompt for PS1's second line, and the input mark.
const BASH_PROMPT: &[u8] =
    b"\x1b]133;A;redraw=last;cl=line\x07\x1b]133;P;k=i\x07line1\r\n\x1b]133;P;k=s\x07$ \x1b]133;B\x07";

#[tokio::test]
async fn prompt_redraws_and_further_lines_start_no_command() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    // A prompt, its redraw, then a command that prints and ends.
    let script = r#"printf '\033]133;A;redraw=last;cl=line\007\033]133;P;k=i\007line1\n\033]133;P;k=s\007$ \033]133;B\007'; printf '\033]133;A;redraw=last;cl=line\007\033]133;P;k=i\007line1\n\033]133;P;k=s\007$ \033]133;B\007'; printf 'ls\n\033]133;C\007out\n\033]133;D;0\007'; printf '\033]133;A\007$ '; exec cat"#;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", script], "cols": 20, "rows": 5}))
        .await;
    let rest = b"ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07";
    let base = 2 * BASH_PROMPT.len() as u64;
    let ran = Command {
        start: past(BASH_PROMPT, b"cl=line\x07"),
        output: Some(base + past(rest, b"133;C\x07")),
        end: Some(base + rest.len() as u64),
        exit: Some(0),
    };
    inspect_until(&mut c, &session.id, |s| s.command == Some(ran)).await;
    let last = read_with(
        &mut c,
        json!({"session": session.id, "scope": "last_command"}),
    )
    .await;
    assert_eq!(last.text, "out\n");
    // The redraw started nothing: every announced command is the one
    // that ran (C and D in one read are one `session_changed`).
    let changed = changed_commands(&mut c, &session.id);
    assert_eq!(changed.last(), Some(&ran));
    assert!(changed.iter().all(|c| c.start == ran.start), "{changed:?}");
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn a_mark_whose_st_is_split_across_reads_ends_after_it() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let script =
        r#"printf '\033]133;C\033'; sleep 0.3; printf '\\output\n\033]133;D;0\007'; exec cat"#;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", script], "cols": 20, "rows": 5}))
        .await;
    let marked = inspect_until(&mut c, &session.id, |s| {
        s.command.is_some_and(|c| c.end.is_some())
    })
    .await;
    assert_eq!(marked.command.unwrap().output, Some(9));
    let last = read_with(
        &mut c,
        json!({"session": session.id, "scope": "last_command"}),
    )
    .await;
    assert_eq!(last.text, "output\n");
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn tail_waits_for_unfinished_sequences_and_characters() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let script = r#"printf 'ab\nx\303'; sleep 0.5; printf '\251y\033]0;never ends'; exec sleep 5"#;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", script], "cols": 20, "rows": 5}))
        .await;
    inspect_until(&mut c, &session.id, |s| s.offset == 6).await;
    // A CR and its LF in two pages are one newline.
    let one = tail(
        &mut c,
        json!({"session": session.id, "after": 0, "max_bytes": 3}),
    )
    .await;
    assert_eq!(
        (one.text.as_str(), one.next_offset, one.complete),
        ("ab\n", 3, false)
    );
    // The half of a character waits; there is nothing more to read now.
    let two = tail(&mut c, json!({"session": session.id, "after": 3})).await;
    assert_eq!(
        (two.text.as_str(), two.next_offset, two.complete),
        ("x", 5, true)
    );
    inspect_until(&mut c, &session.id, |s| s.offset > 6).await;
    let three = tail(&mut c, json!({"session": session.id, "after": 5})).await;
    assert_eq!(three.text, "éy");
    assert!(three.complete, "an open sequence is not more to read");
    let again = tail(
        &mut c,
        json!({"session": session.id, "after": three.next_offset}),
    )
    .await;
    assert_eq!(
        (again.text.as_str(), again.next_offset),
        ("", three.next_offset)
    );
    assert!(again.complete);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn scrollback_counts_a_line_across_the_screen_top_as_the_screen() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "printf 'first\naaaaaaaaaabbbbbbbbbbcc\nx'; exec sleep 5"], "cols": 10, "rows": 3}))
        .await;
    let text = wait_text(&mut c, &session.id, "x").await;
    assert_eq!(text.text, "aaaaaaaaaabbbbbbbbbbcc\nx\n");
    let vt = read_with(&mut c, json!({"session": session.id, "format": "vt"})).await;
    assert_eq!(vt.text, "aaaaaaaaaabbbbbbbbbbcc\r\nx");
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}
