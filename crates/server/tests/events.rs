//! Event numbers and replay, the program's signals as events, `ack` and
//! attention.

mod common;

use std::time::Duration;

use common::*;
use proto::*;
use serde_json::{Value, json};

fn from<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

/// The next event that `pick` takes, skipping the others.
async fn event_where<T>(c: &mut Client, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
    for _ in 0..50 {
        let event = c.event().await;
        if let Some(out) = pick(&event) {
            return out;
        }
    }
    panic!("no matching event within 50 events");
}

/// `send` of `text` to a session running `cat`, which writes it back.
fn typed(id: &str, text: &str) -> Value {
    json!({"session": id, "data": b64(format!("{text}\n"))})
}

fn report(id: &str, body: &str) -> Value {
    typed(id, &format!("\x1b]7501;{body}\x1b\\"))
}

#[tokio::test(flavor = "multi_thread")]
async fn events_are_numbered_and_replayed_after_a_cursor() {
    let server = start(fast_limits());
    let incarnation = server.registry.incarnation.clone();
    let mut a = client(&server).await;
    let first: Subscribed = from(a.ok("subscribe", json!({})).await);
    assert_eq!(first.sessions, Some(vec![]));
    assert_eq!((first.oldest, first.latest, first.gap), (1, 0, None));

    let mut c = client(&server).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    for label in ["one", "two"] {
        c.ok("label", json!({"session": session.id, "label": label}))
            .await;
    }
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(a.numbered().await);
    }
    assert_eq!(seen.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(matches!(seen[0].event, Event::SessionCreated { .. }));

    // After the first event: the other two again, then the live ones.
    let mut b = client(&server).await;
    let replay: Subscribed = from(
        b.ok("subscribe", json!({"after": 1, "incarnation": incarnation}))
            .await,
    );
    assert_eq!(
        (replay.sessions, replay.oldest, replay.latest, replay.gap),
        (None, 1, 3, Some(false))
    );
    assert_eq!(b.numbered().await, seen[1]);
    assert_eq!(b.numbered().await, seen[2]);
    c.ok("label", json!({"session": session.id, "label": "three"}))
        .await;
    let live = a.numbered().await;
    assert_eq!(live.seq, 4);
    assert_eq!(b.numbered().await, live);

    // At the latest: nothing to replay.
    let mut d = client(&server).await;
    let replay: Subscribed = from(
        d.ok("subscribe", json!({"after": 4, "incarnation": incarnation}))
            .await,
    );
    assert_eq!(replay.gap, Some(false));
    c.ok("label", json!({"session": session.id, "label": "four"}))
        .await;
    assert_eq!(d.numbered().await.seq, 5);

    // A cursor above the latest, half a cursor, and a second subscription
    // with one are refused; a second plain one replies with a page.
    let mut e = client(&server).await;
    for params in [
        json!({"after": 99, "incarnation": incarnation}),
        json!({"after": 1}),
        json!({"incarnation": incarnation}),
    ] {
        assert_eq!(
            e.err("subscribe", params.clone()).await,
            ErrorCode::InvalidRequest,
            "{params}"
        );
    }
    assert_eq!(
        d.err("subscribe", json!({"after": 1, "incarnation": incarnation}))
            .await,
        ErrorCode::InvalidRequest
    );
    let again: Subscribed = from(d.ok("subscribe", json!({})).await);
    assert_eq!(
        (again.latest, again.sessions.map(|s| s.len())),
        (5, Some(1))
    );
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
    let exited = d.numbered().await;
    assert_eq!(exited.seq, 6, "each event once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_before_the_retained_events_or_from_another_server_is_a_gap() {
    let server = start(Limits {
        events: 3,
        ..fast_limits()
    });
    let incarnation = server.registry.incarnation.clone();
    let mut c = client(&server).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    for label in ["1", "2", "3", "4", "5"] {
        c.ok("label", json!({"session": session.id, "label": label}))
            .await;
    }
    // Six events, the last three retained.
    let mut b = client(&server).await;
    let reply: Subscribed = from(
        b.ok("subscribe", json!({"after": 2, "incarnation": incarnation}))
            .await,
    );
    assert_eq!((reply.oldest, reply.latest, reply.gap), (4, 6, Some(true)));
    let mut r = client(&server).await;
    let reply: Subscribed = from(
        r.ok("subscribe", json!({"after": 3, "incarnation": incarnation}))
            .await,
    );
    assert_eq!(reply.gap, Some(false), "nothing after 3 was dropped");
    let mut o = client(&server).await;
    let reply: Subscribed = from(
        o.ok("subscribe", json!({"after": 6, "incarnation": "another"}))
            .await,
    );
    assert_eq!(reply.gap, Some(true));

    c.ok("label", json!({"session": session.id, "label": "6"}))
        .await;
    // A gap replays nothing; the live events follow, and the
    // connection serves on.
    assert_eq!(b.numbered().await.seq, 7);
    assert_eq!(o.numbered().await.seq, 7);
    for seq in 4..=7 {
        assert_eq!(r.numbered().await.seq, seq);
    }
    assert_eq!(b.list().await.sessions.len(), 1);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_programs_signals_are_events_at_their_offsets() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let output = b"a\x1b]133;C\x1b\\b\x1b]9;hello\x07\x1b]777;notify;T;B\x07\x1b]9;4;1;40\x07\x1b]9;4;3\x07\x1b]133;D;3\x07c";
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf 'a\\033]133;C\\033\\\\b\\033]9;hello\\007\\033]777;notify;T;B\\007\\033]9;4;1;40\\007\\033]9;4;3\\007\\033]133;D;3\\007c'; exec cat"]}))
        .await;
    let id = session.id.clone();
    let end_of = |needle: &[u8]| {
        let at = output.windows(needle.len()).position(|w| w == needle);
        (at.unwrap() + needle.len()) as u64
    };
    let mut events = Vec::new();
    while events.len() < 6 {
        match c.event().await {
            Event::SessionCreated { .. } => {}
            event => events.push(event),
        }
    }
    assert_eq!(
        events,
        [
            Event::CommandStarted {
                id: id.clone(),
                offset: end_of(b"133;C\x1b\\"),
            },
            Event::Notification {
                id: id.clone(),
                title: String::new(),
                body: "hello".into(),
            },
            Event::Notification {
                id: id.clone(),
                title: "T".into(),
                body: "B".into(),
            },
            Event::Progress {
                id: id.clone(),
                state: ProgressState::Set,
                progress: Some(40),
            },
            Event::Progress {
                id: id.clone(),
                state: ProgressState::Indeterminate,
                progress: None,
            },
            Event::CommandEnded {
                id: id.clone(),
                offset: end_of(b"133;D;3\x07"),
                exit: Some(3),
            },
        ]
    );
    // The BELs that end the sequences rang nothing.
    c.ok("send", typed(&id, "\x1b]9;after\x07")).await;
    let after = event_where(&mut c, |e| match e {
        Event::SessionChanged { .. } => None,
        e => Some(e.clone()),
    })
    .await;
    assert!(matches!(after, Event::Notification { body, .. } if body == "after"));

    c.ok("signal", json!({"session": id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_st_split_across_two_reads_ends_the_sequence_after_it() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf 'ab\\033]133;C\\033'; sleep 0.3; printf '\\\\xyz'; exec cat"]}))
        .await;
    let offset = event_where(&mut c, |e| match e {
        Event::CommandStarted { offset, .. } => Some(*offset),
        _ => None,
    })
    .await;
    // The engine ends the sequence at the ESC; the server holds what it
    // raised until it sees the `\`.
    assert_eq!(offset, b"ab\x1b]133;C\x1b\\".len() as u64);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bells_within_100_ms_are_one() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf '\\007\\007\\007'; sleep 0.5; printf '\\007\\033]9;end\\007'; exec cat"]}))
        .await;
    let mut bells = 0;
    loop {
        match c.event().await {
            Event::Bell { id } => {
                assert_eq!(id, session.id);
                bells += 1;
            }
            Event::Notification { body, .. } if body == "end" => break,
            _ => {}
        }
    }
    assert_eq!(bells, 2);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_open_and_close_as_events() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    let id = session.id.clone();
    let mut o = client(&server).await;
    o.open(3, json!({"session": id, "read_only": true})).await;
    o.bootstrap(3).await;
    let opened = event_where(&mut c, |e| match e {
        Event::AttachmentOpened { .. } => Some(e.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        opened,
        Event::AttachmentOpened {
            id: id.clone(),
            channel: 3,
            client: Peer {
                name: "test".into(),
                version: "0".into()
            },
            read_only: true,
        }
    );
    assert!(
        matches!(c.event().await, Event::SessionChanged { session } if session.attachments == 1)
    );
    o.send(FrameType::Close, 3, b"").await;
    assert_eq!(o.expect_closed(3).await, ClosedReason::Closed);
    assert_eq!(
        c.event().await,
        Event::AttachmentClosed {
            id: id.clone(),
            channel: 3,
            reason: "closed".into(),
        }
    );
    assert!(
        matches!(c.event().await, Event::SessionChanged { session } if session.attachments == 0)
    );

    o.open(4, json!({"session": id})).await;
    o.bootstrap(4).await;
    let opened = event_where(&mut c, |e| match e {
        Event::AttachmentOpened {
            channel, read_only, ..
        } => Some((*channel, *read_only)),
        _ => None,
    })
    .await;
    assert_eq!(opened, (4, false));
    drop(o);
    let closed = event_where(&mut c, |e| match e {
        Event::AttachmentClosed {
            channel, reason, ..
        } => Some((*channel, reason.clone())),
        _ => None,
    })
    .await;
    assert_eq!(closed, (4, "disconnected".into()));
    c.ok("signal", json!({"session": id, "signal": "KILL"}))
        .await;
}

/// The next `program_status_*` event of the session: the record set, or
/// the ids removed, with the reason.
async fn status_event(c: &mut Client) -> (Result<StatusRecord, Vec<String>>, StatusReason) {
    event_where(c, |e| match e {
        Event::ProgramStatusChanged { record, reason, .. } => Some((Ok(record.clone()), *reason)),
        Event::ProgramStatusRemoved {
            record_ids, reason, ..
        } => Some((Err(record_ids.clone()), *reason)),
        _ => None,
    })
    .await
}

/// The attention of the next `session_changed`.
async fn attention_changed(c: &mut Client) -> Option<(String, String)> {
    event_where(c, |e| match e {
        Event::SessionChanged { session } => Some(
            session
                .attention
                .as_ref()
                .map(|a| (a.state.clone(), a.record.clone())),
        ),
        _ => None,
    })
    .await
}

fn attention(state: &str, record: &str) -> Option<(String, String)> {
    Some((state.into(), record.into()))
}

#[tokio::test(flavor = "multi_thread")]
async fn ack_marks_done_and_error_seen_and_attention_follows() {
    let server = start(Limits {
        records: 3,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    let id = session.id.clone();

    c.ok("send", report(&id, "state=working:app=agent")).await;
    let (record, reason) = status_event(&mut c).await;
    let record = record.unwrap();
    assert_eq!(
        (record.state.as_str(), record.seen, reason),
        ("working", None, StatusReason::Report)
    );
    assert_eq!(attention_changed(&mut c).await, attention("working", ""));

    c.ok("send", report(&id, "state=done:id=a")).await;
    let (record, _) = status_event(&mut c).await;
    assert_eq!(record.unwrap().seen, Some(false));
    assert_eq!(attention_changed(&mut c).await, attention("done", "a"));

    c.ok(
        "send",
        report(&id, "state=blocked:kind=question:id=b:msg=R28/"),
    )
    .await;
    assert!(status_event(&mut c).await.0.is_ok());
    assert_eq!(attention_changed(&mut c).await, attention("blocked", "b"));
    let listed = c.list().await.sessions.remove(0).attention.unwrap();
    assert_eq!(
        (
            listed.kind.as_deref(),
            listed.msg.as_deref(),
            listed.app.as_deref()
        ),
        (Some("question"), Some("Go?"), Some("agent")),
        "app from the root"
    );

    c.ok("send", typed(&id, "\x1b]133;A\x1b\\")).await;
    let (removed, reason) = status_event(&mut c).await;
    assert_eq!(
        (removed, reason),
        (Err(vec!["".into(), "b".into()]), StatusReason::Prompt)
    );
    assert_eq!(attention_changed(&mut c).await, attention("done", "a"));

    let acked: Acked = from(
        c.ok("ack", json!({"session": id, "record_ids": ["a", "nope"]}))
            .await,
    );
    assert_eq!(acked.record_ids, ["a"]);
    let (record, reason) = status_event(&mut c).await;
    assert_eq!(
        (record.unwrap().seen, reason),
        (Some(true), StatusReason::Ack)
    );
    assert_eq!(attention_changed(&mut c).await, None);
    let inspected = c.inspect(&id).await;
    assert_eq!(inspected.attention, None);
    assert_eq!(inspected.program_status.unwrap()[0].seen, Some(true));
    let acked: Acked = from(c.ok("ack", json!({"session": id})).await);
    assert!(acked.record_ids.is_empty(), "already seen");

    // A new report on the id is unseen again; a clear removes it.
    c.ok("send", report(&id, "state=error:id=a")).await;
    let (record, _) = status_event(&mut c).await;
    assert_eq!(record.unwrap().seen, Some(false));
    assert_eq!(attention_changed(&mut c).await, attention("error", "a"));
    c.ok("send", report(&id, "state=clear:id=a")).await;
    let (removed, reason) = status_event(&mut c).await;
    assert_eq!(
        (removed, reason),
        (Err(vec!["a".into()]), StatusReason::Clear)
    );
    assert_eq!(attention_changed(&mut c).await, None);

    // Beyond `records` the least recently updated goes.
    for record in ["x", "y", "z", "w"] {
        c.ok("send", report(&id, &format!("state=idle:id={record}")))
            .await;
    }
    let (removed, reason) = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved {
            record_ids, reason, ..
        } => Some((record_ids.clone(), *reason)),
        _ => None,
    })
    .await;
    assert_eq!((removed, reason), (vec!["x".into()], StatusReason::Evicted));

    c.ok("send", report(&id, "state=working:id=v")).await;
    assert_eq!(attention_changed(&mut c).await, attention("working", "v"));
    c.ok("signal", json!({"session": id, "signal": "KILL"}))
        .await;
    let (removed, reason) = status_event(&mut c).await;
    assert_eq!(
        (removed, reason),
        (Err(vec!["v".into()]), StatusReason::Exit)
    );
    let exited = c.wait_exit(&id).await;
    assert_eq!(exited.attention, None, "session_exited carries it");
}

#[tokio::test(flavor = "multi_thread")]
async fn seen_survives_park_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    let session = c
        .create(json!({"argv": ["/bin/cat"], "on_exit": "keep"}))
        .await;
    let id = session.id.clone();
    for body in ["state=done:id=d", "state=error:id=e", "state=working:id=w"] {
        c.ok("send", report(&id, body)).await;
    }
    for _ in 0..1000 {
        if c.inspect(&id).await.program_status.unwrap().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let acked: Acked = from(
        c.ok("ack", json!({"session": id, "record_ids": ["d"]}))
            .await,
    );
    assert_eq!(acked.record_ids, ["d"]);
    drop(c);
    a.registry.stop().await;
    drop(a);

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    let seen = |s: &Session| {
        s.program_status
            .iter()
            .flatten()
            .map(|r| (r.id.clone(), r.seen))
            .collect::<Vec<_>>()
    };
    let loaded = c.inspect(&id).await;
    assert_eq!(loaded.status, Status::Restored);
    assert_eq!(
        seen(&loaded),
        [
            ("d".to_string(), Some(true)),
            ("e".to_string(), Some(false))
        ]
    );
    assert_eq!(
        c.list().await.sessions[0]
            .attention
            .as_ref()
            .map(|a| a.record.as_str()),
        Some("e")
    );
    // The working record dropped at the load is the new incarnation's
    // first event.
    let replay: Subscribed = from(
        c.ok(
            "subscribe",
            json!({"after": 0, "incarnation": b.registry.incarnation}),
        )
        .await,
    );
    assert_eq!(replay.gap, Some(false));
    assert_eq!(
        c.numbered().await,
        Numbered {
            seq: 1,
            event: Event::ProgramStatusRemoved {
                id: id.clone(),
                record_ids: vec!["w".into()],
                reason: StatusReason::Restore,
            }
        }
    );

    // An ack of the restored session is announced and reaches its files.
    c.ok("ack", json!({"session": id})).await;
    let (record, reason) = status_event(&mut c).await;
    assert_eq!(
        (record.unwrap().id.as_str(), reason),
        ("e", StatusReason::Ack)
    );
    assert_eq!(attention_changed(&mut c).await, None);
    let generation = std::fs::read_dir(state.join("sessions").join(&id))
        .unwrap()
        .flatten()
        .find(|g| g.path().join("session.json").is_file())
        .unwrap()
        .path();
    for _ in 0..500 {
        let json = std::fs::read_to_string(generation.join("session.json")).unwrap();
        if !json.contains("\"seen\":false") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(c);
    b.registry.stop().await;
    drop(b);
    let d = start_with(fast_limits(), state);
    let mut c = client(&d).await;
    let loaded = c.inspect(&id).await;
    assert_eq!(
        seen(&loaded),
        [("d".to_string(), Some(true)), ("e".to_string(), Some(true))]
    );
    assert_eq!(loaded.attention, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ack_after_the_exit_of_a_kept_session_parks_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"on_exit": "keep", "argv": ["/bin/sh", "-c",
            "printf '\\033]7501;state=done:id=d\\033\\\\'; exit 0"]}))
        .await;
    c.wait_exit(&session.id).await;
    let parked = |seen: &str| {
        let dir = state.join("sessions").join(&session.id);
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|g| {
                std::fs::read_to_string(g.path().join("session.json"))
                    .is_ok_and(|json| json.contains(seen))
            })
    };
    for _ in 0..500 {
        if parked("\"seen\":false") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(parked("\"seen\":false"), "the park at the exit");
    c.ok("ack", json!({"session": session.id})).await;
    for _ in 0..500 {
        if parked("\"seen\":true") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the ack was not parked");
}

#[tokio::test(flavor = "multi_thread")]
async fn retained_events_are_bounded_in_bytes_too() {
    let server = start(Limits {
        max_frame: 4096,
        ..fast_limits()
    });
    let incarnation = server.registry.incarnation.clone();
    let mut c = client(&server).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    for n in 0..40 {
        c.ok(
            "label",
            json!({"session": session.id, "label": format!("label {n}")}),
        )
        .await;
    }
    let mut b = client(&server).await;
    let reply: Subscribed = from(
        b.ok("subscribe", json!({"after": 1, "incarnation": incarnation}))
            .await,
    );
    assert_eq!((reply.latest, reply.gap), (41, Some(true)));
    let retained = reply.latest + 1 - reply.oldest;
    assert!(retained > 0 && retained < 40, "{retained} retained");
    let mut r = client(&server).await;
    r.ok(
        "subscribe",
        json!({"after": reply.oldest - 1, "incarnation": incarnation}),
    )
    .await;
    for seq in reply.oldest..=reply.latest {
        assert_eq!(r.numbered().await.seq, seq);
    }
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ack_of_a_running_session_reaches_its_next_park() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let timing = server::ParkTiming {
        interval: Duration::from_secs(300),
        quiet: Duration::from_millis(50),
        gap: Duration::from_millis(50),
        busy: Duration::from_millis(500),
    };
    let a = Server::start_with_park(fast_limits(), state.clone(), timing);
    let mut c = client(&a).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    c.ok("send", report(&session.id, "state=done:id=d")).await;
    let parked = |seen: &str| {
        let dir = state.join("sessions").join(&session.id);
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|g| {
                std::fs::read_to_string(g.path().join("session.json"))
                    .is_ok_and(|json| json.contains(seen))
            })
    };
    let wait = |seen: &'static str| async move {
        for _ in 0..500 {
            if parked(seen) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    };
    assert!(wait("\"seen\":false").await, "the report parked");
    c.ok("ack", json!({"session": session.id})).await;
    assert!(wait("\"seen\":true").await, "the ack parked");
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removal_closes_its_attachments_as_removed() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"on_exit": "keep", "argv": ["/bin/sh", "-c", "exit 0"]}))
        .await;
    c.wait_exit(&session.id).await;
    let mut o = client(&server).await;
    o.open(2, json!({"session": session.id, "read_only": true}))
        .await;
    o.bootstrap(2).await;
    c.wait_attachments(&session.id, 1).await;
    c.ok("remove", json!({"session": session.id})).await;
    assert_eq!(o.expect_closed(2).await, ClosedReason::Removed);
    let reason = event_where(&mut c, |e| match e {
        Event::AttachmentClosed { reason, .. } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert_eq!(reason, "removed");
}
