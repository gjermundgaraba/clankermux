//! The attachments list, `detach`, and the size policies.

mod common;

use std::time::Duration;

use common::*;
use proto::*;
use serde_json::{Value, json};

/// A client whose `HELLO` names it `name` at `version`, with the engine.
async fn client_named(server: &Server, name: &str, version: &str) -> Client {
    let mut c = Client::connect(server).await;
    c.send_json(
        FrameType::Hello,
        0,
        &json!({"version": 1, "client": {"name": name, "version": version},
                "engine": ghostty_vt::ENGINE, "capabilities": []}),
    )
    .await;
    let (header, body) = c.recv_json().await;
    assert_eq!(header.frame_type, FrameType::Welcome);
    c.max_frame = serde_json::from_value::<Welcome>(body)
        .unwrap()
        .limits
        .max_frame;
    c
}

/// The next `session_changed` for `id` whose session `pick` accepts.
async fn changed_until(c: &mut Client, id: &str, pick: impl Fn(&Session) -> bool) -> Session {
    for _ in 0..50 {
        if let Event::SessionChanged { session } = c.event().await
            && session.id == id
            && pick(&session)
        {
            return session;
        }
    }
    panic!("no matching session_changed within 50 events");
}

/// Polls `inspect` until `pick` accepts the session.
async fn inspect_until(c: &mut Client, id: &str, pick: impl Fn(&Session) -> bool) -> Session {
    for _ in 0..500 {
        let session = c.inspect(id).await;
        if pick(&session) {
            return session;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("inspect never matched: {:?}", c.inspect(id).await);
}

/// `CLOSED` on `channel`, after any stream frames before it.
async fn closed_on(c: &mut Client, channel: u32) -> Closed {
    loop {
        let (header, payload) = c.recv_on(channel).await;
        if header.frame_type == FrameType::Closed {
            return serde_json::from_slice(&payload).unwrap();
        }
    }
}

fn size_of(s: &Session) -> (u16, u16) {
    (s.cols, s.rows)
}

fn viewport(cols: u16, rows: u16) -> Option<Cells> {
    Some(Cells { cols, rows })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_list_follows_open_input_viewport_and_close() {
    let server = start(fast_limits());
    let mut ctl = client(&server).await;
    ctl.ok("subscribe", json!({})).await;
    let session = ctl
        .create(json!({"argv": ["/bin/cat"], "cols": 20, "rows": 5}))
        .await;
    assert!(session.attachments.is_empty());
    assert_eq!(session.size, SizeInfo::default());

    let mut one = client_named(&server, "one\u{1}", &"9".repeat(40)).await;
    one.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 30, "rows": 8}}),
    )
    .await;
    let boot = one.bootstrap(1).await;
    let opened = changed_until(&mut ctl, &session.id, |s| s.attachments.len() == 1).await;
    let first = &opened.attachments[0];
    assert_eq!(
        (&first.client.name, first.client.version.len()),
        (&"one?".to_string(), 32),
        "the HELLO client, cut to 32 bytes, control characters replaced"
    );
    assert!(!first.read_only);
    assert_eq!(first.viewport, viewport(30, 8));
    assert!(first.last_input.is_none());
    assert!(!first.since.is_empty());
    assert_eq!(
        opened.size.owner,
        Some(first.channel),
        "the latest report is the opener's"
    );
    assert_eq!(
        boot.attached.session.attachments, opened.attachments,
        "ATTACHED carries the list with the new attachment"
    );

    let mut two = client_named(&server, "two", "2").await;
    two.open(1, json!({"session": session.id, "read_only": true}))
        .await;
    two.bootstrap(1).await;
    let both = changed_until(&mut ctl, &session.id, |s| s.attachments.len() == 2).await;
    let second = &both.attachments[1];
    assert_eq!(both.attachments[0].channel, first.channel, "in open order");
    assert_ne!(
        second.channel, first.channel,
        "unique on the server, though both use channel 1"
    );
    assert_eq!(
        (
            second.client.name.as_str(),
            second.read_only,
            second.viewport
        ),
        ("two", true, None)
    );

    // Input sets `last_input` without an event.
    one.send(FrameType::Input, 1, b"x").await;
    inspect_until(&mut ctl, &session.id, |s| {
        s.attachments[0].last_input.is_some()
    })
    .await;

    // A viewport report that changes nothing but the recorded viewport
    // still changes the list.
    one.send_json(FrameType::Viewport, 1, &json!({"cols": 40, "rows": 10}))
        .await;
    let reported = changed_until(&mut ctl, &session.id, |s| {
        s.attachments[0].viewport == viewport(40, 10)
    })
    .await;
    assert_eq!(size_of(&reported), (40, 10));

    one.send(FrameType::Close, 1, b"").await;
    assert_eq!(closed_on(&mut one, 1).await.reason, ClosedReason::Closed);
    let closed = changed_until(&mut ctl, &session.id, |s| s.attachments.len() == 1).await;
    assert_eq!(closed.attachments[0].channel, second.channel);
    assert_eq!(closed.size.owner, None, "the owner went with it");
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_closes_another_connections_attachment() {
    let server = start(fast_limits());
    let mut viewer = client(&server).await;
    let session = viewer.create(json!({"argv": ["/bin/cat"]})).await;
    viewer.open(3, json!({"session": session.id})).await;
    viewer.bootstrap(3).await;

    let mut ctl = Client::connect(&server).await;
    ctl.hello_with(None).await;
    let channel = ctl.inspect(&session.id).await.attachments[0].channel;
    ctl.ok(
        "detach",
        json!({"session": session.id, "channel": channel, "reason": "taken over"}),
    )
    .await;
    assert_eq!(
        closed_on(&mut viewer, 3).await,
        Closed {
            reason: ClosedReason::Detached,
            message: Some("taken over".into()),
        }
    );
    ctl.wait_attachments(&session.id, 0).await;
    // The channel is free again.
    viewer.open(3, json!({"session": session.id})).await;
    viewer.bootstrap(3).await;

    let detach = |channel: Value| json!({"session": session.id, "channel": channel});
    assert_eq!(
        ctl.err("detach", detach(json!(channel))).await,
        ErrorCode::NoSuchAttachment,
        "a channel is never reused"
    );
    assert_eq!(
        ctl.err("detach", json!({"session": "nope", "channel": channel}))
            .await,
        ErrorCode::NoSuchSession
    );
    let reopened = ctl.inspect(&session.id).await.attachments[0].channel;
    assert_eq!(
        ctl.err(
            "detach",
            json!({"session": session.id, "channel": reopened,
                   "reason": "x".repeat(fast_limits().text as usize + 1)})
        )
        .await,
        ErrorCode::LimitExceeded
    );
    // Without a reason, no message; an exited session's attachment too.
    ctl.ok("end", json!({"session": session.id})).await;
    inspect_until(&mut ctl, &session.id, |s| s.status == Status::Exited).await;
    ctl.ok("detach", detach(json!(reopened))).await;
    assert_eq!(
        closed_on(&mut viewer, 3).await,
        Closed {
            reason: ClosedReason::Detached,
            message: None
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_policy_with_two_attachments() {
    let server = start(fast_limits());
    let mut a = client(&server).await;
    let mut b = client(&server).await;
    // Requests from a connection with no attachment: `a` and `b` leave
    // their stream frames unread.
    let mut ctl = client(&server).await;
    let session = ctl
        .create(json!({"argv": ["/bin/cat"], "cols": 20, "rows": 5}))
        .await;
    let id = session.id.as_str();
    a.open(
        1,
        json!({"session": id, "viewport": {"cols": 100, "rows": 30}}),
    )
    .await;
    a.bootstrap(1).await;
    b.open(
        1,
        json!({"session": id, "viewport": {"cols": 80, "rows": 40}}),
    )
    .await;
    b.bootstrap(1).await;
    let s = ctl.inspect(id).await;
    let (ca, cb) = (s.attachments[0].channel, s.attachments[1].channel);

    // latest: the most recent report, and who made it.
    assert_eq!((size_of(&s), s.size.owner), ((80, 40), Some(cb)));
    a.send_json(FrameType::Viewport, 1, &json!({"cols": 100, "rows": 30}))
        .await;
    let s = inspect_until(&mut ctl, id, |s| size_of(s) == (100, 30)).await;
    assert_eq!(
        s.size,
        SizeInfo {
            policy: SizePolicy::Latest,
            owner: Some(ca)
        }
    );

    // smallest: the least columns and the least rows, at once.
    ctl.ok("resize", json!({"session": id, "policy": "smallest"}))
        .await;
    let s = ctl.inspect(id).await;
    assert_eq!(
        (size_of(&s), s.size.policy, s.size.owner),
        ((80, 30), SizePolicy::Smallest, None)
    );
    a.send_json(FrameType::Viewport, 1, &json!({"cols": 120, "rows": 50}))
        .await;
    inspect_until(&mut ctl, id, |s| size_of(s) == (80, 40)).await;
    b.send_json(FrameType::Viewport, 1, &json!({"cols": 60, "rows": 20}))
        .await;
    inspect_until(&mut ctl, id, |s| size_of(s) == (60, 20)).await;
    // `resize` holds until the next report.
    ctl.ok("resize", json!({"session": id, "cols": 200, "rows": 60}))
        .await;
    assert_eq!(size_of(&ctl.inspect(id).await), (200, 60));
    a.send_json(FrameType::Viewport, 1, &json!({"cols": 120, "rows": 50}))
        .await;
    inspect_until(&mut ctl, id, |s| size_of(s) == (60, 20)).await;

    // fixed: reports are recorded, not applied; `resize` applies.
    ctl.ok("resize", json!({"session": id, "policy": "fixed"}))
        .await;
    a.send_json(FrameType::Viewport, 1, &json!({"cols": 70, "rows": 25}))
        .await;
    let s = inspect_until(&mut ctl, id, |s| {
        s.attachments[0].viewport == viewport(70, 25)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let s2 = ctl.inspect(id).await;
    assert_eq!(
        (size_of(&s), size_of(&s2), s2.size.owner),
        ((60, 20), (60, 20), None)
    );
    ctl.ok("resize", json!({"session": id, "cols": 90, "rows": 25}))
        .await;
    assert_eq!(size_of(&ctl.inspect(id).await), (90, 25));

    // smallest again, then the smaller one leaves.
    ctl.ok("resize", json!({"session": id, "policy": "smallest"}))
        .await;
    assert_eq!(size_of(&ctl.inspect(id).await), (60, 20));
    b.send(FrameType::Close, 1, b"").await;
    assert_eq!(closed_on(&mut b, 1).await.reason, ClosedReason::Closed);
    inspect_until(&mut ctl, id, |s| size_of(s) == (70, 25)).await;

    // A policy and a size in one request: the policy, then the size.
    ctl.ok(
        "resize",
        json!({"session": id, "policy": "fixed", "cols": 33}),
    )
    .await;
    let s = ctl.inspect(id).await;
    assert_eq!((size_of(&s), s.size.policy), ((33, 25), SizePolicy::Fixed));

    assert_eq!(
        ctl.err("resize", json!({"session": id})).await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        ctl.err("resize", json!({"session": id, "policy": "largest"}))
            .await,
        ErrorCode::InvalidRequest
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_policy_is_parked_and_restored() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 30, "rows": 6}}),
    )
    .await;
    c.bootstrap(1).await;
    c.ok("resize", json!({"session": session.id, "policy": "fixed"}))
        .await;
    a.registry.stop().await;
    a.shutdown();
    drop(c);
    drop(a);

    let b = start_with(fast_limits(), state);
    let mut c = client(&b).await;
    let restored = c.inspect(&session.id).await;
    assert_eq!(restored.status, Status::Restored);
    assert_eq!(
        restored.size,
        SizeInfo {
            policy: SizePolicy::Fixed,
            owner: None
        }
    );
    assert!(restored.attachments.is_empty());
    assert_eq!(
        c.err("resize", json!({"session": session.id, "policy": "latest"}))
            .await,
        ErrorCode::NotRunning
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn opens_beyond_the_limit_are_refused() {
    let server = start(Limits {
        attachments: 2,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    for channel in 1..=2 {
        c.open(channel, json!({"session": session.id})).await;
        c.bootstrap(channel).await;
    }
    c.open(3, json!({"session": session.id})).await;
    assert_eq!(c.expect_error(3).await, ErrorCode::LimitExceeded);
    c.send(FrameType::Close, 1, b"").await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
    c.wait_attachments(&session.id, 1).await;
    c.open(3, json!({"session": session.id})).await;
    c.bootstrap(3).await;
}
