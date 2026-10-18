//! The client library against an in-process server: bootstrap, stream,
//! reconnect with resume, events.

use std::path::PathBuf;
use std::time::Duration;

use client::{Action, Attachment, Connection, Frame, connection::hello};
use proto::*;
use serde_json::json;
use tokio::sync::mpsc;

struct Server {
    socket: PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("sock");
    let registry = server::Registry::new(server::Config {
        limits: Limits {
            remove_grace: 200,
            ..Limits::default()
        },
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
    });
    let task = tokio::spawn(server::serve_unix(socket.clone(), registry));
    // The socket file exists before the server listens on it.
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Server {
        socket,
        _dir: dir,
        task,
    }
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

/// Feeds frames until `done` holds, returning every action.
async fn drive(
    a: &mut Attachment,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
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
    let server = start().await;
    let conn = Connection::connect_unix(&server.socket, hello("test", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap();
    assert_eq!(conn.welcome.engine, ghostty_vt::ENGINE);
    let session: Session = conn
        .call(
            "create",
            json!({"argv": ["/bin/cat"], "cols": 20, "rows": 5}),
        )
        .await
        .unwrap();
    conn.request("write", json!({"session": session.id, "data": "b25lCg=="}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

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
    let mut rx = conn.open(1, &open).unwrap();
    let mut a = Attachment::new(
        &conn.welcome.incarnation,
        conn.welcome.limits.continuation as usize,
    );
    let actions = drive(&mut a, &mut rx, |a, _| {
        !a.bootstrapping() && a.term().is_some()
    })
    .await;
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
    let conn = Connection::connect_unix(&server.socket, hello("test", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap();
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
    loop {
        if let Event::SessionExited { session: s } = events.recv().await.unwrap() {
            assert_eq!(s.id, session.id);
            break;
        }
    }
    assert_eq!(a.resume(), None);

    // Server errors arrive typed; the connection reports its end.
    let err = conn.request("inspect", json!({"session": "nope"})).await;
    assert!(matches!(err, Err(client::Error::Server(e)) if e.code == ErrorCode::NoSuchSession));
    // A second HELLO is a protocol error: the server answers without an
    // id and closes, which the connection reports.
    conn.send_json(FrameType::Hello, 0, &hello("test", None))
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed())
        .await
        .unwrap();
    assert!(
        matches!(&closed, client::Error::Protocol(m) if m.contains("ProtocolError")),
        "{closed}"
    );
    assert!(matches!(
        conn.request("list", json!({})).await,
        Err(client::Error::Closed)
    ));
}
