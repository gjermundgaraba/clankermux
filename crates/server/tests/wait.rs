//! `send` with text, keys, paste and a wait; `wait`; `cancel`
//! (PROTOCOL.md, Control).

mod common;

use std::time::Duration;

use common::*;
use proto::*;
use serde_json::{Value, json};

/// A session that prints `setup` (escape sequences that set modes), then
/// `ready`, then the next `count` bytes it reads, as hex.
async fn byte_reader(c: &mut Client, setup: &str, count: usize) -> Session {
    let script = format!(
        "stty raw -echo; printf '%s' \"$SETUP\"; printf 'ready\\r\\n'; \
         dd bs=1 count={count} 2>/dev/null | od -An -tx1; printf 'end\\r\\n'; exec cat"
    );
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c", script], "env": {"SETUP": setup}}))
        .await;
    wait_text(c, &s.id, "ready").await;
    s
}

/// The bytes a `send` with `params` writes, as the program read them.
async fn written(c: &mut Client, setup: &str, count: usize, params: Value) -> String {
    let s = byte_reader(c, setup, count).await;
    let mut params = params;
    params["session"] = json!(s.id);
    c.ok("send", params).await;
    let read = wait_text(c, &s.id, "end").await;
    let after = read.text.split_once("ready").unwrap().1;
    let hex = after.split_once("end").unwrap().0;
    hex.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn keys_are_encoded_by_the_session_terminal_modes() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    // Cursor keys: normal, then application mode (DECCKM).
    assert_eq!(
        written(&mut c, "", 3, json!({"keys": ["up"]})).await,
        "1b 5b 41"
    );
    assert_eq!(
        written(&mut c, "\x1b[?1h", 3, json!({"keys": ["up"]})).await,
        "1b 4f 41"
    );
    // Kitty keyboard, disambiguate: ctrl+c is CSI 99;5u.
    assert_eq!(
        written(&mut c, "\x1b[>1u", 7, json!({"keys": ["ctrl+c"]})).await,
        "1b 5b 39 39 3b 35 75"
    );
}

#[tokio::test]
async fn keys_in_one_request_are_one_write_and_a_bad_name_writes_nothing() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    assert_eq!(
        written(
            &mut c,
            "",
            8,
            json!({"keys": ["a", "B", "ctrl+a", "tab", "shift+tab", "enter"]})
        )
        .await,
        "61 42 01 09 1b 5b 5a 0d"
    );
    let s = c.create(json!({"argv": ["/bin/cat"]})).await;
    let before = c.inspect(&s.id).await.offset;
    let error = c
        .request("send", json!({"session": s.id, "keys": ["a", "hyper+q"]}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("hyper+q"), "{}", error.message);
    for bad in [
        json!({"session": s.id}),
        json!({"session": s.id, "text": "a", "data": "YQ=="}),
        json!({"session": s.id, "keys": ["a"], "paste": true}),
        json!({"session": s.id, "text": "a", "wait": {"until": {}}}),
        json!({"session": s.id, "text": "a", "wait": {"until": {"output": {"regex": "("}}}}),
    ] {
        assert_eq!(
            c.err("send", bad.clone()).await,
            ErrorCode::InvalidRequest,
            "{bad}"
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(c.inspect(&s.id).await.offset, before, "nothing was written");
}

#[tokio::test]
async fn a_paste_is_bracketed_only_when_the_program_asked() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    assert_eq!(
        written(&mut c, "", 3, json!({"text": "a\nb", "paste": true})).await,
        "61 0d 62"
    );
    assert_eq!(
        written(
            &mut c,
            "\x1b[?2004h",
            15,
            json!({"text": "a\nb", "paste": true})
        )
        .await,
        "1b 5b 32 30 30 7e 61 0a 62 1b 5b 32 30 31 7e"
    );
    // Without paste, text goes as typed, bracket or not.
    assert_eq!(
        written(&mut c, "\x1b[?2004h", 3, json!({"text": "a\nb"})).await,
        "61 0a 62"
    );
}

/// `wait` and its reply.
async fn wait(c: &mut Client, params: Value) -> Waited {
    serde_json::from_value(c.ok("wait", params).await).unwrap()
}

#[tokio::test]
async fn each_until_fires() {
    let server = start(fast_limits());
    let mut c = client(&server).await;

    // exited, with the exit.
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c", "sleep 0.3; exit 3"], "on_exit": "keep"}))
        .await;
    let w = wait(&mut c, json!({"session": s.id, "until": {"exited": true}})).await;
    assert_eq!(
        (w.reason, w.exit),
        (WaitReason::Exited, Some(Exit::Code { code: 3 }))
    );
    let ended = c.inspect(&s.id).await;
    assert_eq!((w.at.offset, w.at.marker), (ended.offset, ended.marker));

    // output, text and regex, escape sequences aside.
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c",
                                 "sleep 0.3; printf 'one \\033[31mtwo\\033[m three\\r\\n'; exec cat"]}))
        .await;
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"output": {"text": "two three"}}}),
    )
    .await;
    assert_eq!(w.reason, WaitReason::Matched);
    let m = w.matched.unwrap();
    assert_eq!(m.text, "two three");
    // "one " is 4 bytes, then the 5-byte colour.
    assert_eq!(m.offset, 9);
    assert_eq!(w.at.offset, 9 + 3 + 3 + 6);
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"output": {"regex": "o+ne \\w+"}}, "after": {"offset": 0, "marker": 0}}),
    )
    .await;
    assert_eq!(w.matched.unwrap().text, "one two");

    // status, by state and by record.
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "sleep 0.3; printf '\\033]7501;state=working:id=a\\033\\\\'; sleep 0.2; printf '\\033]7501;state=done:id=b\\033\\\\'; exec cat"]}))
        .await;
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"status": {"states": ["done", "error"]}}}),
    )
    .await;
    assert_eq!(w.reason, WaitReason::Status);
    assert_eq!(w.record.unwrap().id, "b");
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"status": {"states": ["working"], "record": "a"}}}),
    )
    .await;
    assert_eq!(w.record.unwrap().state, "working", "already holds: at once");

    // prompt: the next OSC 133 A.
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf 'before\\r\\n'; sleep 0.4; printf '\\033]133;A\\007$ '; exec cat"]}))
        .await;
    wait_text(&mut c, &s.id, "before").await;
    let w = wait(&mut c, json!({"session": s.id, "until": {"prompt": true}})).await;
    assert_eq!(w.reason, WaitReason::Prompt);
    let now = c.inspect(&s.id).await;
    assert!(w.at.offset > 8 && w.at.offset <= now.offset, "{w:?}");
    // From before it, it already holds.
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"prompt": true}, "after": {"offset": 0, "marker": 0}}),
    )
    .await;
    assert_eq!(w.reason, WaitReason::Prompt);

    // quiet, and timeout as a result.
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c", "printf a; sleep 0.3; printf b; exec cat"]}))
        .await;
    let started = std::time::Instant::now();
    let w = wait(&mut c, json!({"session": s.id, "until": {"quiet_ms": 200}})).await;
    assert_eq!(w.reason, WaitReason::Quiet);
    assert!(started.elapsed() >= Duration::from_millis(200));
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"output": {"text": "never"}}, "timeout_ms": 100}),
    )
    .await;
    assert_eq!(w.reason, WaitReason::Timeout);
}

#[tokio::test]
async fn facts_that_already_hold_fire_at_once_and_an_ended_stream_is_not_running() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c", "printf 'hello there\\r\\n'; exit 4"], "on_exit": "keep"}))
        .await;
    c.wait_exit(&s.id).await;
    let zero = json!({"offset": 0, "marker": 0});
    let w = wait(&mut c, json!({"session": s.id, "until": {"exited": true}})).await;
    assert_eq!(w.exit, Some(Exit::Code { code: 4 }));
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"output": {"text": "there"}}, "after": zero}),
    )
    .await;
    assert_eq!(w.matched.unwrap().offset, 6);
    for until in [
        json!({"output": {"text": "absent"}}),
        json!({"prompt": true}),
        json!({"quiet_ms": 10}),
    ] {
        assert_eq!(
            c.err(
                "wait",
                json!({"session": s.id, "until": until, "after": zero})
            )
            .await,
            ErrorCode::NotRunning,
            "{until}"
        );
    }
    assert_eq!(
        c.err(
            "wait",
            json!({"session": "nope", "until": {"exited": true}})
        )
        .await,
        ErrorCode::NoSuchSession
    );
    assert_eq!(
        c.err(
            "wait",
            json!({"session": s.id, "until": {"exited": true}, "after": {"offset": 1u64 << 40, "marker": 0}})
        )
        .await,
        ErrorCode::InvalidRequest,
        "after past the cursor"
    );
}

#[tokio::test]
async fn send_with_a_wait_catches_what_the_program_prints_at_once() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    // The terminal echoes the text as soon as it is written.
    for _ in 0..20 {
        let s = c.create(json!({"argv": ["/bin/cat"]})).await;
        let sent: Sent = serde_json::from_value(
            c.ok(
                "send",
                json!({"session": s.id, "text": "marco\r",
                       "wait": {"until": {"output": {"regex": "mar+co"}}, "timeout_ms": 5000}}),
            )
            .await,
        )
        .unwrap();
        let waited = sent.wait.clone().unwrap();
        assert_eq!(waited.reason, WaitReason::Matched);
        assert_eq!(waited.matched.unwrap().offset, 0);
        // The cursor when the text was queued, before the echo.
        assert_eq!(sent.at.offset, 0, "{sent:?}");
    }
    // A plain send replies with its cursor.
    let s = c.create(json!({"argv": ["/bin/cat"]})).await;
    let sent: Sent =
        serde_json::from_value(c.ok("send", json!({"session": s.id, "text": "x"})).await).unwrap();
    assert!(sent.wait.is_none());
}

#[tokio::test]
async fn a_send_waits_for_a_status_set_after_it() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    // `done` from the turn before, then a new turn on each line read.
    let s = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf '\\033]7501;state=done:id=a\\033\\\\'; read x; \
             printf '\\033]7501;state=working:id=b\\033\\\\'; sleep 0.3; \
             printf '\\033]7501;state=done:id=b\\033\\\\'; exec cat"]}))
        .await;
    let w = wait(
        &mut c,
        json!({"session": s.id, "until": {"status": {"states": ["done"]}}}),
    )
    .await;
    assert_eq!(w.record.unwrap().id, "a", "a wait takes the record there");
    let sent: Sent = serde_json::from_value(
        c.ok(
            "send",
            json!({"session": s.id, "text": "go\r",
                   "wait": {"until": {"status": {"states": ["done"]}}, "timeout_ms": 5000}}),
        )
        .await,
    )
    .unwrap();
    let waited = sent.wait.unwrap();
    assert_eq!(waited.reason, WaitReason::Status);
    assert_eq!(waited.record.unwrap().id, "b", "a send's, only a later one");
}

#[tokio::test]
async fn cancel_ends_a_wait_and_its_reply_comes_last() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let s = c.create(json!({"argv": ["/bin/cat"]})).await;
    c.send_json(
        FrameType::Request,
        0,
        &json!({"id": 100, "method": "wait", "params": {"session": s.id, "until": {"exited": true}}}),
    )
    .await;
    c.send_json(
        FrameType::Request,
        0,
        &json!({"id": 101, "method": "send", "params": {"session": s.id, "text": "y",
                 "wait": {"until": {"output": {"text": "never"}}}}}),
    )
    .await;
    // Other requests are served meanwhile, a send to the same session
    // too.
    c.list().await;
    c.inspect(&s.id).await;
    let sent: Sent =
        serde_json::from_value(c.ok("send", json!({"session": s.id, "text": "z"})).await).unwrap();
    assert!(sent.wait.is_none());
    for (cancel, target) in [(200, 100), (201, 101)] {
        c.send_json(
            FrameType::Request,
            0,
            &json!({"id": cancel, "method": "cancel", "params": {"id": target}}),
        )
        .await;
        let (header, first) = c.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Error, "{first}");
        assert_eq!(
            (first["id"].as_i64(), first["code"].as_str()),
            (Some(target), Some("cancelled"))
        );
        let (header, second) = c.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Reply);
        assert_eq!(second, json!({"id": cancel, "result": {"cancelled": true}}));
    }
    assert_eq!(
        c.ok("cancel", json!({"id": 100})).await,
        json!({"cancelled": false})
    );
    assert_eq!(
        c.ok("cancel", json!({"id": 9999})).await,
        json!({"cancelled": false})
    );
    // Nothing else arrives for them: the session ends and nothing fires.
    // Subscribed first: an exit before the subscription is not replayed.
    c.ok("subscribe", json!({})).await;
    c.ok("end", json!({"session": s.id})).await;
    c.wait_exit(&s.id).await;
}
