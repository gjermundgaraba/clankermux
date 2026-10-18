//! Conformance tests for the Control section of PROTOCOL.md, driven over
//! a Unix socket against an in-process server.

mod common;

use std::time::Duration;

use common::*;
use proto::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

/// What `read` may return under `max_frame` 8192: the server keeps the
/// rest for the JSON around the text.
const TEXT_BUDGET: usize = 8192 - 4096;

#[tokio::test]
async fn handshake_and_protocol_errors() {
    let server = start(fast_limits());

    let mut c = Client::connect(&server).await;
    let welcome = c.hello().await;
    assert_eq!(welcome.version, 1);
    assert_eq!(welcome.engine, ghostty_vt::ENGINE);
    assert_eq!(welcome.limits.page, 2);
    c.send_json(
        FrameType::Hello,
        0,
        &json!({"version": 1, "client": {"name": "t", "version": "0"}}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error");
    assert!(
        c.recv().await.is_none(),
        "connection closes after a second HELLO"
    );

    let mut c = Client::connect(&server).await;
    c.send_json(
        FrameType::Hello,
        0,
        &json!({"version": 2, "client": {"name": "t", "version": "0"}}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "version_mismatch");
    assert!(c.recv().await.is_none());

    let mut c = Client::connect(&server).await;
    c.send_json(
        FrameType::Request,
        0,
        &json!({"id": 1, "method": "list", "params": {}}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error", "request before HELLO");
    assert!(c.recv().await.is_none());

    let mut c = client(&server).await;
    c.send(FrameType::Request, 3, b"{}").await;
    let (header, body) = c.recv_json().await;
    assert_eq!(
        (header.channel, body["code"].as_str()),
        (0, Some("protocol_error")),
        "REQUEST on channel 3"
    );
    assert!(c.recv().await.is_none());

    let mut c = client(&server).await;
    c.stream
        .write_all(&[0x98, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error", "unknown type");
    assert!(c.recv().await.is_none());

    let mut c = client(&server).await;
    let big = vec![b' '; Limits::default().control_body as usize + 1];
    c.send(FrameType::Request, 0, &big).await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error", "JSON above control_body");

    let mut c = client(&server).await;
    c.send(FrameType::Input, 9, b"dropped").await;
    assert_eq!(
        c.err("nope", json!({})).await,
        ErrorCode::InvalidRequest,
        "unknown method after a dropped frame"
    );

    let mut c = Client::connect(&server).await;
    assert!(
        c.recv().await.is_none(),
        "closed without HELLO within hello_timeout"
    );
}

#[tokio::test]
async fn session_lifecycle() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    let first: Page = serde_json::from_value(events.ok("subscribe", json!({})).await).unwrap();
    assert!(first.sessions.is_empty());

    let session = c.create(json!({"label": "cat", "argv": ["/bin/cat"], "cols": 30, "rows": 10, "on_exit": "keep"})).await;
    assert_eq!(session.status, Status::Running);
    assert_eq!((session.cols, session.rows), (30, 10));
    assert_eq!(session.argv, ["/bin/cat"]);
    assert!(session.pid.is_some());
    assert_eq!(session.on_exit, OnExit::Keep);
    assert_eq!(session.marker, 0);
    assert!(
        matches!(events.event().await, Event::SessionCreated { session: s } if s.id == session.id)
    );

    let data = b64("hello\n");
    c.ok("write", json!({"session": session.id, "data": data}))
        .await;
    let read = wait_text(&mut c, &session.id, "hello\nhello").await;
    assert_eq!(read.text, "hello\nhello\n", "echo and cat's copy");
    assert_eq!(read.cursor, Some(Cursor { x: 0, y: 2 }));
    assert!(read.offset > 0);

    let listed = c.list().await;
    assert_eq!(listed.sessions[0].offset, read.offset);
    assert!(listed.sessions[0].program_status.is_none());
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.program_status, Some(vec![]));

    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 40, "rows": 12}),
    )
    .await;
    assert!(
        matches!(events.event().await, Event::SessionChanged { session: s } if (s.cols, s.rows, s.marker) == (40, 12, 1))
    );
    // A title set by the program is announced, and so is its clearing
    // by a full reset (RIS), which the engine reports separately.
    // `cat` passes a line on once it is complete.
    let data = b64("\x1b]2;titled\x07\n");
    c.ok("write", json!({"session": session.id, "data": data}))
        .await;
    assert!(
        matches!(events.event().await, Event::SessionChanged { session: s } if s.title == "titled")
    );
    let data = b64("\x1bc\n");
    c.ok("write", json!({"session": session.id, "data": data}))
        .await;
    assert!(
        matches!(events.event().await, Event::SessionChanged { session: s } if s.title.is_empty())
    );
    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 40, "rows": 12}),
    )
    .await;

    let relabelled: Session = serde_json::from_value(
        c.ok("set_label", json!({"session": session.id, "label": "new"}))
            .await,
    )
    .unwrap();
    assert_eq!(relabelled.label, "new");
    assert!(
        matches!(events.event().await, Event::SessionChanged { session: s } if s.label == "new")
    );
    assert_eq!(
        c.err(
            "set_label",
            json!({"session": session.id, "label": "x".repeat(257)})
        )
        .await,
        ErrorCode::LimitExceeded
    );

    assert_eq!(
        c.err("remove", json!({"session": session.id})).await,
        ErrorCode::Running
    );
    assert_eq!(
        c.err("signal", json!({"session": session.id, "signal": "BOGUS"}))
            .await,
        ErrorCode::InvalidRequest
    );
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
    let exited = events.wait_exit(&session.id).await;
    assert_eq!(exited.status, Status::Exited);
    assert_eq!(
        exited.exit,
        Some(Exit::Signal {
            signal: "KILL".into()
        })
    );
    assert_eq!(exited.marker, 2);
    assert!(exited.pid.is_none());

    assert_eq!(
        c.err("write", json!({"session": session.id, "data": ""}))
            .await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err("end", json!({"session": session.id})).await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err(
            "set_size",
            json!({"session": session.id, "cols": 1, "rows": 1})
        )
        .await,
        ErrorCode::NotRunning
    );
    let read = c.read(&session.id, 5).await;
    assert_eq!(read.status, Status::Exited);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        c.ok("inspect", json!({"session": session.id}))
            .await
            .is_object(),
        "on_exit keep leaves it listed"
    );

    c.ok("remove", json!({"session": session.id})).await;
    assert!(matches!(events.event().await, Event::SessionRemoved { id } if id == session.id));
    assert_eq!(
        c.err("inspect", json!({"session": session.id})).await,
        ErrorCode::NoSuchSession
    );
}

#[tokio::test]
async fn exit_code_and_removal_on_exit() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/sh", "-c", "exit 3"]})).await;
    assert!(matches!(c.event().await, Event::SessionCreated { .. }));
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(exited.exit, Some(Exit::Code { code: 3 }));
    assert!(exited.exited_at.is_some());
    let read = c.read(&session.id, 0).await;
    assert_eq!(read.status, Status::Exited);
    assert!(
        matches!(c.event().await, Event::SessionRemoved { id } if id == session.id),
        "removed after remove_grace"
    );
    assert_eq!(
        c.err("read", json!({"session": session.id})).await,
        ErrorCode::NoSuchSession
    );
}

#[tokio::test]
async fn end_sends_hangup() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(
            json!({"argv": ["/bin/sh", "-c", "trap 'exit 7' HUP; echo ready; while :; do sleep 0.05; done"]}),
        )
        .await;
    wait_text(&mut c, &session.id, "ready").await;
    c.ok("end", json!({"session": session.id})).await;
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(exited.exit, Some(Exit::Code { code: 7 }));
}

#[tokio::test]
async fn create_defaults_and_limits() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c.create(json!({})).await;
    assert_eq!(session.argv, ["/bin/sh"]);
    assert_eq!((session.cols, session.rows), (80, 24));
    assert_eq!(session.on_exit, OnExit::Remove);
    assert_eq!(session.label, "");
    assert_eq!(
        c.err("create", json!({"label": "x".repeat(300)})).await,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        c.err("create", json!({"cwd": "x".repeat(3000)})).await,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        c.err("create", json!({"argv": ["/nonexistent/program"]}))
            .await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        c.err("create", json!({"cols": "wide"})).await,
        ErrorCode::InvalidRequest
    );
    for bad in [
        json!({"argv": []}),
        json!({"argv": ["/bin/sh", "a\0b"]}),
        json!({"cwd": "/tmp\0"}),
        json!({"env": {"A": "x\0"}}),
    ] {
        assert_eq!(
            c.err("create", bad.clone()).await,
            ErrorCode::InvalidRequest,
            "{bad}"
        );
    }

    let env = c.create(json!({"argv": ["/bin/sh", "-c", "printf '%s|%s' \"$TERM\" \"$FOO\"; sleep 10"], "env": {"FOO": "bar"}})).await;
    let mut text = String::new();
    for _ in 0..500 {
        text = c.read(&env.id, 0).await.text;
        if text.contains('|') {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(text.trim_end(), "xterm-ghostty|bar");
    c.ok("signal", json!({"session": env.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn list_pages_by_id() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..5 {
        ids.push(c.create(json!({"argv": ["/bin/sleep", "5"]})).await.id);
    }
    ids.sort();
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page: Page =
            serde_json::from_value(c.ok("list", json!({"after": after})).await).unwrap();
        assert!(page.sessions.len() <= 2);
        seen.extend(page.sessions.iter().map(|s| s.id.clone()));
        match page.next {
            Some(next) => {
                assert_eq!(&next, seen.last().unwrap());
                after = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(seen, ids);
    for id in ids {
        c.ok("signal", json!({"session": id, "signal": "KILL"}))
            .await;
    }
}

#[tokio::test]
async fn events_are_ordered_with_replies() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/sleep", "5"]})).await;
    // The server emits the event before it sends the reply, and this
    // connection receives both in that order: the event was queued while
    // waiting for the reply.
    assert_eq!(c.events.len(), 1, "event before reply");
    assert!(matches!(c.event().await, Event::SessionCreated { session: s } if s.id == session.id));
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
    c.wait_exit(&session.id).await;
}

#[tokio::test]
async fn read_returns_scrollback_then_screen() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=1; while [ $i -le 50 ]; do echo line$i; i=$((i+1)); done; sleep 5"], "cols": 20, "rows": 5}))
        .await;
    let mut lines = Vec::new();
    for _ in 0..500 {
        let read = c.read(&session.id, 3).await;
        lines = read.text.lines().map(str::to_string).collect();
        if lines.last().is_some_and(|l| l == "line50") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The screen holds line47 to line50 and the blank cursor row.
    assert_eq!(lines.len(), 7, "{lines:?}");
    assert_eq!(lines[0], "line44");
    let read: Read =
        serde_json::from_value(c.ok("read", json!({"session": session.id})).await).unwrap();
    assert_eq!(read.text.lines().count(), 4);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn malformed_requests_pipelining_and_the_hello_deadline() {
    let server = start(fast_limits());

    // HELLO and a request in one write are both handled.
    let mut c = Client::connect(&server).await;
    let mut both = encode_frame(
        FrameType::Hello,
        0,
        br#"{"version":1,"client":{"name":"t","version":"0"}}"#,
    );
    both.extend(encode_frame(
        FrameType::Request,
        0,
        br#"{"id":1,"method":"list","params":{}}"#,
    ));
    c.stream.write_all(&both).await.unwrap();
    assert_eq!(c.recv_json().await.0.frame_type, FrameType::Welcome);
    let (header, body) = c.recv_json().await;
    assert_eq!(
        (header.frame_type, body["id"].as_i64()),
        (FrameType::Reply, Some(1))
    );

    // A request without an integer id gets an id-less error, then the
    // connection closes.
    c.send(
        FrameType::Request,
        0,
        br#"{"id":"x","method":"list","params":{}}"#,
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "invalid_request");
    assert!(body.get("id").is_none());
    assert!(c.recv().await.is_none());

    // A malformed request with an id is answered and the connection stays.
    let mut c = client(&server).await;
    c.send(FrameType::Request, 0, br#"{"id":-7,"method":"list"}"#)
        .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(
        (body["code"].as_str(), body["id"].as_i64()),
        (Some("invalid_request"), Some(-7))
    );
    assert!(c.request("list", json!({})).await.is_ok());

    // Another version's shape still gets version_mismatch.
    let mut c = Client::connect(&server).await;
    c.send_json(FrameType::Hello, 0, &json!({"version": 2, "peer": "x"}))
        .await;
    assert_eq!(c.recv_json().await.1["code"], "version_mismatch");
    assert!(c.recv().await.is_none());

    // The HELLO deadline runs from connecting, not from the last byte.
    let mut c = Client::connect(&server).await;
    let hello = encode_frame(
        FrameType::Hello,
        0,
        br#"{"version":1,"client":{"name":"t","version":"0"}}"#,
    );
    let started = std::time::Instant::now();
    c.stream.write_all(&hello[..1]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.stream.write_all(&hello[1..2]).await.unwrap();
    assert!(
        c.recv().await.is_none(),
        "closed although bytes kept arriving"
    );
    // A deadline reset by the second byte would close at 500 ms.
    assert!(started.elapsed() < Duration::from_millis(450));
}

#[tokio::test]
async fn write_replies_come_in_request_order() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo up; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "up").await;
    for _ in 0..20 {
        let mut pipelined = Vec::new();
        for id in 1..=4 {
            let data = b64([b'0' + id as u8]);
            let request = json!({"id": id, "method": "write",
                "params": {"session": session.id, "data": data}});
            pipelined.extend(encode_frame(
                FrameType::Request,
                0,
                &serde_json::to_vec(&request).unwrap(),
            ));
        }
        c.stream.write_all(&pipelined).await.unwrap();
        let mut ids = Vec::new();
        while ids.len() < 4 {
            let (header, body) = c.recv_json().await;
            if header.frame_type == FrameType::Reply {
                ids.push(body["id"].as_i64().unwrap());
            }
        }
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }
}

#[tokio::test]
async fn subscribed_connections_close_on_errors_and_get_each_event_once() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    c.ok("subscribe", json!({})).await;
    let mut other = client(&server).await;
    let session = other.create(json!({"argv": ["/bin/sleep", "5"]})).await;
    assert!(matches!(c.event().await, Event::SessionCreated { session: s } if s.id == session.id));
    let big = vec![b' '; Limits::default().control_body as usize + 1];
    c.send(FrameType::Request, 0, &big).await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error");
    assert!(c.recv().await.is_none(), "closed although subscribed");
    other
        .ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn signals_by_name_and_sizes_are_validated() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "kill -SEGV $$"], "on_exit": "keep"}))
        .await;
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(
        exited.exit,
        Some(Exit::Signal {
            signal: "SEGV".into()
        })
    );
    let session = c.create(json!({"argv": ["/bin/sleep", "5"]})).await;
    assert_eq!(
        c.err(
            "set_size",
            json!({"session": session.id, "cols": 0, "rows": 1})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    c.ok(
        "signal",
        json!({"session": session.id, "signal": "SIGALRM"}),
    )
    .await;
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(
        exited.exit,
        Some(Exit::Signal {
            signal: "ALRM".into()
        })
    );
    assert_eq!(
        c.err("create", json!({"cols": 0})).await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        c.err(
            "create",
            json!({"argv": ["/bin/sh", "-c", "exit 0"], "cwd": "/nonexistent"})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    let term = c
        .create(json!({"argv": ["/bin/sh", "-c", "printf '%s' \"$TERM\"; sleep 1"], "env": {"TERM": "dumb"}}))
        .await;
    assert_eq!(wait_text(&mut c, &term.id, "dumb").await.text, "dumb\n");
    c.ok("signal", json!({"session": term.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn read_screen_only_after_clear_and_bounded_replies() {
    let server = start(Limits {
        max_frame: 8192,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=1; while [ $i -le 1000 ]; do echo line$i; i=$((i+1)); done; printf '\\033[H\\033[2Jonly\\n'; sleep 5"], "cols": 20, "rows": 5}))
        .await;
    let read = wait_text(&mut c, &session.id, "only").await;
    assert_eq!(read.text, "only\n", "no history without scrollback");
    let read = c.read(&session.id, 2).await;
    // Before the clear the screen held line997 to line1000 and the cursor row.
    assert_eq!(read.text, "line995\nline996\nonly\n");
    let read = c.read(&session.id, 100000).await;
    assert!(read.text.len() <= TEXT_BUDGET);
    assert!(
        read.text.starts_with("line"),
        "whole lines are dropped: {:?}",
        &read.text[..10]
    );
    assert!(read.text.ends_with("line996\nonly\n"));
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;

    // One line longer than the budget: its newest characters are kept.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=0; while [ $i -lt 6000 ]; do printf x; i=$((i+1)); done; printf 'END\n'; sleep 5"], "cols": 200, "rows": 5}))
        .await;
    let read = wait_text(&mut c, &session.id, "END").await;
    assert!(read.text.len() <= TEXT_BUDGET);
    assert!(
        read.text.ends_with("xxxEND\n"),
        "{:?}",
        &read.text[read.text.len() - 10..]
    );
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn a_connection_that_stops_reading_is_closed() {
    // The queue ceiling is 256 KiB plus four frames; small frames keep
    // the test short.
    let server = start(Limits {
        max_frame: 16 * 1024,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let mut idle = client(&server).await;
    idle.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    let label = "x".repeat(200);
    for _ in 0..2000 {
        c.ok("set_label", json!({"session": session.id, "label": label}))
            .await;
    }
    // The idle subscriber reads what the socket buffered, then end of file.
    while idle.recv().await.is_some() {}
}
