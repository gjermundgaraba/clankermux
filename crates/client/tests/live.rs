//! The client library against an in-process server: bootstrap, stream,
//! reconnect with resume, events.

use std::time::Duration;

use client::{Action, Attachment, Connection, Frame, connection::hello};
use proto::*;
use serde_json::json;
use testkit::Server;

/// A test's whole wait for a condition.
const DEADLINE: Duration = Duration::from_secs(10);

fn start() -> Server {
    Server::start(Limits {
        remove_grace: 200,
        ..Limits::default()
    })
}

async fn connect(server: &Server) -> Connection {
    Connection::connect_unix(&server.socket, hello("test", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap()
}

fn text(a: &mut Attachment) -> String {
    let out = ghostty_vt::formatter::format(
        a.term().unwrap(),
        ghostty_vt::formatter::Options {
            trim: true,
            ..Default::default()
        },
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

/// Polls `read` until the session's screen holds `needle`.
async fn wait_screen(conn: &Connection, session: &str, needle: &str) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let read: Read = conn
            .call("read", json!({"session": session}))
            .await
            .unwrap();
        if read.text.contains(needle) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{:?}", read.text);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Feeds frames until `done` holds, returning every action.
async fn drive(
    a: &mut Attachment,
    rx: &mut client::Receiver<Frame>,
    mut done: impl FnMut(&mut Attachment, &[Action]) -> bool,
) -> Vec<Action> {
    let mut all = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a frame within 5 s")
            .expect("channel open");
        let actions = a.handle(frame).unwrap();
        all.extend(actions);
        if done(a, &all) {
            return all;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_stream_and_resume() {
    let server = start();
    let conn = connect(&server).await;
    assert_eq!(conn.welcome.engine, ghostty_vt::ENGINE);
    let session: Session = conn
        .call(
            "create",
            json!({"argv": ["/bin/cat"], "cols": 20, "rows": 5}),
        )
        .await
        .unwrap();
    conn.request("send", json!({"session": session.id, "data": "b25lCg=="}))
        .await
        .unwrap();
    // The echo is on the screen before the attach, so it comes with the
    // snapshot rather than as OUTPUT.
    wait_screen(&conn, &session.id, "one\none").await;

    let open = Open {
        session: session.id.clone(),
        read_only: false,
        viewport: Some(Size {
            cols: 20,
            rows: 5,
            cell_width: None,
            cell_height: None,
        }),
        profile: None,
        resume: None,
    };
    let mut a = Attachment::new();
    a.open_on(&conn.welcome);
    let mut rx = conn.open(1, &open).unwrap();
    let actions = drive(&mut a, &mut rx, |a, _| a.resume().is_some()).await;
    assert_eq!(actions, vec![Action::Screen]);
    assert_eq!(text(&mut a), "one\none");
    assert_eq!(a.session.as_ref().unwrap().id, session.id);

    conn.send(FrameType::Input, 1, b"two\n").unwrap();
    drive(&mut a, &mut rx, |a, _| text(a).contains("two\ntwo")).await;
    let cursor = a.resume().unwrap();
    assert!(cursor.offset > 0);

    // The connection drops; a new one resumes from the cursor and the
    // terminal is kept.
    drop(conn);
    let conn = connect(&server).await;
    a.open_on(&conn.welcome);
    let mut rx = conn
        .open(
            1,
            &Open {
                resume: Some(cursor),
                ..open.clone()
            },
        )
        .unwrap();
    let actions = drive(&mut a, &mut rx, |a, _| a.resume().is_some()).await;
    assert_eq!(actions, vec![]);
    assert_eq!(text(&mut a), "one\none\ntwo\ntwo");
    conn.send(FrameType::Input, 1, b"three\n").unwrap();
    drive(&mut a, &mut rx, |a, _| text(a).contains("three\nthree")).await;

    // A resize marker, then the exit.
    let mut events = conn.events();
    conn.request("subscribe", json!({})).await.unwrap();
    conn.send_json(
        FrameType::Viewport,
        1,
        &Viewport {
            size: Size {
                cols: 30,
                rows: 6,
                cell_width: None,
                cell_height: None,
            },
            profile: None,
        },
    )
    .unwrap();
    let actions = drive(&mut a, &mut rx, |_, all| {
        all.iter().any(|x| matches!(x, Action::Resized { .. }))
    })
    .await;
    assert!(actions.contains(&Action::Resized { cols: 30, rows: 6 }));
    assert_eq!(a.term().unwrap().cols(), 30);
    conn.request("signal", json!({"session": session.id, "signal": "KILL"}))
        .await
        .unwrap();
    let actions = drive(&mut a, &mut rx, |a, _| a.ended()).await;
    assert_eq!(
        actions.last(),
        Some(&Action::Exited(Exit::Signal {
            signal: "KILL".into()
        }))
    );
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("session_exited in time")
            .unwrap();
        if let Event::SessionExited { session: s } = event.event {
            assert_eq!(s.id, session.id);
            break;
        }
    }
    assert_eq!(a.resume(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_errors_are_typed_and_a_protocol_error_ends_the_connection() {
    let server = start();
    let conn = connect(&server).await;
    let mut events = conn.events();
    conn.request("subscribe", json!({})).await.unwrap();
    // Server errors arrive typed; the connection reports its end.
    let err = conn.request("inspect", json!({"session": "nope"})).await;
    assert!(matches!(err, Err(client::Error::Server(e)) if e.code == ErrorCode::NoSuchSession));
    // A second HELLO is a protocol error: the server answers without an
    // id and closes, which the connection reports.
    conn.send_json(FrameType::Hello, 0, &hello("test", None))
        .unwrap();
    // The event stream ends with the connection, which says why.
    tokio::time::timeout(Duration::from_secs(5), async {
        while events.recv().await.is_some() {}
    })
    .await
    .unwrap();
    let closed = conn.ended().expect("ended before its events");
    assert!(
        matches!(&closed, client::Error::Server(e) if e.code == ErrorCode::ProtocolError),
        "{closed}"
    );
    // Requests after the end fail with the reason, not a bare "closed".
    assert!(matches!(
        conn.request("list", json!({})).await,
        Err(client::Error::Server(e)) if e.code == ErrorCode::ProtocolError
    ));
    assert!(matches!(
        conn.open(
            2,
            &Open {
                session: "nope".into(),
                read_only: false,
                viewport: None,
                profile: None,
                resume: None,
            }
        ),
        Err(client::Error::Server(e)) if e.code == ErrorCode::ProtocolError
    ));
    assert!(conn.events().recv().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpolled_attachment_is_bounded_and_recovers_while_another_keeps_up() {
    let server = start();
    let conn = connect(&server).await;
    let session: Session = conn
        .call("create", json!({"argv": ["/bin/sleep", "60"]}))
        .await
        .unwrap();
    let open = Open {
        session: session.id.clone(),
        read_only: true,
        viewport: None,
        profile: None,
        resume: None,
    };
    let mut slow = conn.open(1, &open).unwrap();
    let mut a = Attachment::new();
    a.open_on(&conn.welcome);
    drive(&mut a, &mut slow, |a, _| a.resume().is_some()).await;
    let mut fast = conn.open(2, &open).unwrap();
    while fast.recv().await.unwrap().header.frame_type != FrameType::SnapshotEnd {}
    let shared = server.registry.get(&session.id).unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    let mut received = 0;
    for _ in 0..100 {
        shared.lock().unwrap().output(&chunk);
        let end = received + chunk.len();
        while received < end {
            let frame = tokio::time::timeout(DEADLINE, fast.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame.header.frame_type, FrameType::Output);
            let (offset, bytes) = parse_output(&frame.payload).unwrap();
            assert_eq!(offset, received as u64);
            assert!(bytes.iter().all(|&b| b == b'x'));
            received += bytes.len();
        }
    }
    assert!(conn.ended().is_none());
    conn.request("inspect", json!({"session": session.id}))
        .await
        .unwrap();
    let mut held = 0;
    let mut lost = false;
    while let Some(frame) = tokio::time::timeout(DEADLINE, slow.recv()).await.unwrap() {
        for action in a.handle(frame).unwrap() {
            match action {
                Action::Output(bytes) => held += bytes.len(),
                Action::Error(error) => {
                    assert_eq!(error.code, ErrorCode::TooSlow);
                    lost = true;
                }
                other => panic!("unexpected action: {other:?}"),
            }
        }
    }
    assert!(lost);
    assert!(held <= 4 * 1024 * 1024, "queued {held} bytes");
    assert!(a.ended() && a.resume().is_none());
    let mut fresh = conn.open(3, &open).unwrap();
    a.open_on(&conn.welcome);
    drive(&mut a, &mut fresh, |a, _| a.resume().is_some()).await;
    assert_eq!(a.resume().unwrap().offset, received as u64);
    conn.send(FrameType::Close, 2, &[]).unwrap();
    conn.send(FrameType::Close, 3, &[]).unwrap();
}
