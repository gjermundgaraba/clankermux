//! Strict in, open out (PROTOCOL.md, Conventions): every payload a client
//! sends refuses a field the protocol does not define, and the payloads
//! the 0.2.0 web package sends are all defined.

mod common;

use common::*;
use proto::*;
use serde_json::{Value, json};

/// `value` with `"extra": 1` added at the object `path` leads to.
fn with_extra(mut value: Value, path: &[&str]) -> Value {
    let mut at = &mut value;
    for key in path {
        at = &mut at[*key];
    }
    at["extra"] = json!(1);
    value
}

#[tokio::test]
async fn an_unknown_field_in_hello_closes_the_connection() {
    let server = start(fast_limits());
    let hello = json!({"version": 1, "client": {"name": "t", "version": "0"}, "capabilities": []});
    for path in [&[][..], &["client"][..]] {
        let mut c = Client::connect(&server).await;
        c.send_json(FrameType::Hello, 0, &with_extra(hello.clone(), path))
            .await;
        let (header, body) = c.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Error);
        assert_eq!(body["code"], "invalid_request", "{path:?}: {body}");
        assert!(body.get("id").is_none());
        assert!(c.recv().await.is_none(), "{path:?}: closed after");
    }
}

#[tokio::test]
async fn an_unknown_field_in_a_request_is_invalid() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let s = c.create(json!({"argv": ["/bin/cat"]})).await;
    let id = s.id.as_str();

    c.send_json(
        FrameType::Request,
        0,
        &json!({"id": 7, "method": "list", "params": {}, "extra": 1}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(
        (body["id"].as_i64(), body["code"].as_str()),
        (Some(7), Some("invalid_request"))
    );

    let requests = [
        ("create", json!({"argv": ["/bin/cat"]})),
        ("list", json!({})),
        ("subscribe", json!({})),
        ("inspect", json!({"session": id})),
        ("end", json!({"session": id})),
        ("signal", json!({"session": id, "signal": "CONT"})),
        ("remove", json!({"session": id})),
        ("label", json!({"session": id, "label": "x"})),
        (
            "resize",
            json!({"session": id, "cols": 81, "policy": "fixed"}),
        ),
        (
            "detach",
            json!({"session": id, "channel": 1, "reason": "x"}),
        ),
        ("read", json!({"session": id})),
        ("send", json!({"session": id, "data": ""})),
        ("ack", json!({"session": id})),
        ("subscribe", json!({"after": 0, "incarnation": "x"})),
        ("describe", json!({})),
        ("describe", json!({"method": "send"})),
    ];
    for (method, params) in requests {
        assert_eq!(
            c.err(method, with_extra(params, &[])).await,
            ErrorCode::InvalidRequest,
            "{method}"
        );
    }
    // The objects inside them, and `wait` and `cancel`.
    let until = json!({"output": {"text": "x"}, "status": {"states": ["done"]}});
    let nested = [
        ("wait", json!({"session": id, "until": until}), &[][..]),
        (
            "wait",
            json!({"session": id, "until": until}),
            &["until"][..],
        ),
        (
            "wait",
            json!({"session": id, "until": until}),
            &["until", "output"][..],
        ),
        (
            "wait",
            json!({"session": id, "until": until}),
            &["until", "status"][..],
        ),
        (
            "wait",
            json!({"session": id, "until": until, "after": {"offset": 0, "marker": 0}}),
            &["after"][..],
        ),
        (
            "send",
            json!({"session": id, "text": "x", "wait": {"until": until}}),
            &["wait"][..],
        ),
        (
            "send",
            json!({"session": id, "text": "x", "wait": {"until": until}}),
            &["wait", "until"][..],
        ),
        ("cancel", json!({"id": 99}), &[][..]),
    ];
    for (method, params, path) in nested {
        assert_eq!(
            c.err(method, with_extra(params, path)).await,
            ErrorCode::InvalidRequest,
            "{method} {path:?}"
        );
    }
    assert_eq!(
        c.err("subscribe", json!({"after": id})).await,
        ErrorCode::InvalidRequest,
        "subscribe takes no after"
    );
    // Nothing above took effect: the session runs on unlabelled.
    let after = c.inspect(id).await;
    assert_eq!(
        (
            after.status,
            after.label.as_str(),
            after.cols,
            after.size.policy
        ),
        (Status::Running, "", 80, SizePolicy::Latest)
    );
}

#[tokio::test]
async fn an_unknown_field_in_open_or_viewport_is_invalid() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let s = c.create(json!({"argv": ["/bin/cat"]})).await;
    let open = json!({
        "session": s.id,
        "viewport": {"cols": 80, "rows": 24},
        "profile": {"scheme": "dark"},
        "resume": {"incarnation": "x", "offset": 0, "marker": 0},
    });
    for path in [&[][..], &["viewport"], &["profile"], &["resume"]] {
        c.open(1, with_extra(open.clone(), path)).await;
        assert_eq!(
            c.expect_error(1).await,
            ErrorCode::InvalidRequest,
            "OPEN {path:?}"
        );
    }
    let viewport = json!({"cols": 80, "rows": 24, "profile": {"scheme": "dark"}});
    for path in [&[][..], &["profile"]] {
        c.open(1, json!({"session": s.id})).await;
        c.bootstrap(1).await;
        c.send_json(FrameType::Viewport, 1, &with_extra(viewport.clone(), path))
            .await;
        assert_eq!(
            c.expect_error(1).await,
            ErrorCode::InvalidRequest,
            "VIEWPORT {path:?} ends the attachment"
        );
    }
}

/// The payload of the next `frame_type` on `channel`; the login shell's
/// output on any channel meanwhile is skipped.
async fn until(c: &mut Client, channel: u32, frame_type: FrameType) -> Vec<u8> {
    loop {
        let (header, payload) = c.recv().await.expect("a frame");
        if (header.channel, header.frame_type) == (channel, frame_type) {
            return payload;
        }
        assert_eq!(header.frame_type, FrameType::Output, "{header:?}");
    }
}

/// The payloads of `@gjermundgaraba/clankermux` 0.2.0, as its
/// `connection.ts` and `worker.ts` build them, verbatim: a 0.2.0 page and
/// this server work together, except for a paste (`write`).
#[tokio::test]
async fn the_payloads_of_the_0_2_0_web_package_are_accepted() {
    let server = start(fast_limits());
    let mut c = Client::connect(&server).await;
    // `hello("clankermux-web", ENGINE)`.
    c.send_json(
        FrameType::Hello,
        0,
        &json!({"version": 1, "client": {"name": "clankermux-web", "version": "0.2.0"},
                "capabilities": [], "engine": ghostty_vt::ENGINE}),
    )
    .await;
    let (header, _) = c.recv_json().await;
    assert_eq!(header.frame_type, FrameType::Welcome);

    // `subscribe` with `{}`, then `list {after}` for the next page; on
    // a second connection, so the events stay off this one.
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;
    // `create` at the page's size, `on_exit` when given.
    let kept: Session = serde_json::from_value(
        c.ok(
            "create",
            json!({"cols": 100, "rows": 30, "on_exit": "keep"}),
        )
        .await,
    )
    .unwrap();
    let plain: Session =
        serde_json::from_value(c.ok("create", json!({"cols": 100, "rows": 30})).await).unwrap();
    c.ok("list", json!({"after": kept.id})).await;

    // OPEN while active: viewport and profile, no resume yet.
    let profile = json!({"scheme": "dark", "fg": "#d8dee9", "bg": "#282c34", "cursor": "#ffffff"});
    c.open(
        1,
        json!({"session": kept.id,
               "viewport": {"cols": 90, "rows": 20, "cell_width": 16, "cell_height": 34},
               "profile": profile}),
    )
    .await;
    let attached = c.bootstrap(1).await.attached;
    // VIEWPORT: the size with the cell size, and the profile.
    c.send_json(
        FrameType::Viewport,
        1,
        &json!({"cols": 91, "rows": 21, "cell_width": 16, "cell_height": 34, "profile": profile}),
    )
    .await;
    let resized: Resized =
        serde_json::from_slice(&until(&mut c, 1, FrameType::Resized).await).unwrap();
    assert_eq!(resized.cols, 91);
    // OPEN while hidden, resuming: no viewport or profile.
    c.open(
        2,
        json!({"session": kept.id,
               "resume": {"incarnation": server.registry.incarnation,
                          "offset": attached.offset, "marker": attached.marker}}),
    )
    .await;
    let resumed: Attached =
        serde_json::from_slice(&until(&mut c, 2, FrameType::Attached).await).unwrap();
    assert_eq!(resumed.mode, AttachMode::Resume);

    // `end`, then `remove` once exited.
    c.send(FrameType::Close, 1, b"").await;
    c.send(FrameType::Close, 2, b"").await;
    let mut open = 2;
    while open > 0 {
        let (header, _) = c.recv().await.expect("a frame");
        if header.frame_type == FrameType::Closed {
            open -= 1;
        }
    }
    c.ok("end", json!({"session": kept.id})).await;
    events.wait_exit(&kept.id).await;
    c.ok("remove", json!({"session": kept.id})).await;

    // The one refusal: 0.2.0 pastes with `write`, now `send`.
    assert_eq!(
        c.err("write", json!({"session": plain.id, "data": "aGkK"}))
            .await,
        ErrorCode::InvalidRequest
    );
}
