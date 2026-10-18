//! Loss testing: several sessions and clients through a proxy that delays
//! and cuts connections. A carrier is an ordered reliable byte stream, so
//! packet loss reaches the protocol as delay, stalls and dropped
//! connections; the proxy produces exactly those. Every client must end
//! with the server's screen for its session.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use client::{Action, Attachment, Endpoint, connection::hello};
use proto::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const SESSIONS: usize = 8;

/// A TCP proxy that delays every chunk, drops every connection on
/// `cut`, and refuses new ones while `paused`.
struct Proxy {
    addr: SocketAddr,
    delay_ms: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    cuts: Arc<AtomicUsize>,
    connections: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let delay_ms = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(AtomicBool::new(false));
        let cuts = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = {
            let delay_ms = delay_ms.clone();
            let paused = paused.clone();
            let connections = connections.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((client, _)) = listener.accept().await else {
                        return;
                    };
                    if paused.load(Ordering::Relaxed) {
                        drop(client);
                        continue;
                    }
                    let Ok(server) = TcpStream::connect(upstream).await else {
                        continue;
                    };
                    let (cr, cw) = client.into_split();
                    let (sr, sw) = server.into_split();
                    let delay = delay_ms.clone();
                    let pair = tokio::spawn(async move {
                        let _ = tokio::join!(
                            relay(cr, sw, delay.clone()),
                            relay(sr, cw, delay.clone()),
                        );
                    });
                    connections.lock().unwrap().push(pair.abort_handle());
                }
            })
        };
        Proxy {
            addr,
            delay_ms,
            paused,
            cuts,
            connections,
            task,
        }
    }

    /// Drops every connection in flight; clients see a closed socket.
    fn cut(&self) {
        for handle in self.connections.lock().unwrap().drain(..) {
            handle.abort();
        }
        self.cuts.fetch_add(1, Ordering::Relaxed);
    }
}

/// Copies one direction, holding each chunk for the current delay.
async fn relay(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    delay_ms: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let delay = delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

struct Server {
    tcp: SocketAddr,
    _dir: tempfile::TempDir,
    _tasks: JoinSet<std::io::Result<()>>,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let registry = server::Registry::new(server::Config {
        limits: Limits::default(),
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
        state_dir: dir.path().join("state"),
    })
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp = listener.local_addr().unwrap();
    let mut tasks = JoinSet::new();
    tasks.spawn(server::serve_tcp(listener, registry));
    Server {
        tcp,
        _dir: dir,
        _tasks: tasks,
    }
}

/// The screen as the client's engine shows it, trimmed, history excluded.
fn screen(a: &mut Attachment) -> String {
    let term = a.term().unwrap();
    let from = term.scrollback_rows() as u32;
    let out = ghostty_vt::formatter::format(
        term,
        ghostty_vt::formatter::Options {
            trim: true,
            region: ghostty_vt::formatter::Region::FromRow(from),
            ..Default::default()
        },
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

/// What one client saw.
#[derive(Default, Debug)]
struct Seen {
    reconnects: usize,
    resumes: usize,
    snapshots: usize,
    gaps: usize,
    errors: Vec<ErrorCode>,
}

/// Runs `follow` on its own thread: the engine behind an attachment is
/// not `Send`.
fn follow_thread(
    endpoint: Endpoint,
    session: String,
    done: String,
) -> std::thread::JoinHandle<(String, Seen)> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut a, seen) = follow(endpoint, session, &done).await;
                (screen(&mut a), seen)
            })
    })
}

/// Attaches to `session` through `endpoint` until its screen shows
/// `done`, reconnecting with resume whenever the connection drops.
async fn follow(endpoint: Endpoint, session: String, done: &str) -> (Attachment, Seen) {
    let mut a = Attachment::new();
    let mut seen = Seen::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{session}: {seen:?}"
        );
        let conn = match endpoint
            .connect(hello("loss", Some(ghostty_vt::ENGINE)))
            .await
        {
            Ok(conn) => conn,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        a.open_on(&conn.welcome);
        let resume = a.resume();
        let Ok(mut rx) = conn.open(
            1,
            &Open {
                session: session.clone(),
                read_only: false,
                viewport: Some(Size {
                    cols: 80,
                    rows: 24,
                    cell_width: None,
                    cell_height: None,
                }),
                profile: None,
                resume,
            },
        ) else {
            continue;
        };
        if seen.reconnects > 0 || seen.snapshots > 0 {
            seen.reconnects += 1;
        }
        let mut first = true;
        loop {
            let Some(frame) = rx.recv().await else {
                break;
            };
            if first && frame.header.frame_type == FrameType::Attached {
                first = false;
                let attached: Attached = frame.json().unwrap();
                match attached.mode {
                    AttachMode::Resume => seen.resumes += 1,
                    AttachMode::Snapshot => seen.snapshots += 1,
                }
            }
            let actions = match a.handle(frame) {
                Ok(actions) => actions,
                Err(_) => break,
            };
            for action in actions {
                match action {
                    Action::Gap(_) => seen.gaps += 1,
                    Action::Error(error) => seen.errors.push(error.code),
                    _ => {}
                }
            }
            if a.term().is_some() && screen(&mut a).contains(done) {
                return (a, seen);
            }
        }
        // Lost: reconnect.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clients_converge_through_delay_and_cuts() {
    let server = start().await;
    let proxy = Proxy::start(server.tcp).await;
    let direct = Endpoint::Tcp(server.tcp.to_string());
    let control = direct
        .connect(hello("control", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap();
    let mut sessions = Vec::new();
    for n in 0..SESSIONS {
        let script = format!(
            "i=0; while [ $i -lt 300 ]; do echo \"line $i of session {n}\"; i=$((i+1)); sleep 0.01; done; echo DONE-{n}; exec cat"
        );
        let session: Session = control
            .call(
                "create",
                json!({"argv": ["/bin/sh", "-c", script], "label": format!("s{n}")}),
            )
            .await
            .unwrap();
        sessions.push(session.id);
    }

    proxy.delay_ms.store(5, Ordering::Relaxed);
    let through = Endpoint::Tcp(proxy.addr.to_string());
    let clients: Vec<_> = sessions
        .iter()
        .enumerate()
        .map(|(n, id)| follow_thread(through.clone(), id.clone(), format!("DONE-{n}")))
        .collect();
    // Cuts while the output flows, then a calm end.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        proxy.cut();
    }
    proxy.delay_ms.store(0, Ordering::Relaxed);

    let results = tokio::task::spawn_blocking(move || {
        clients
            .into_iter()
            .map(|c| c.join().unwrap())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert_eq!(results.len(), SESSIONS);
    let mut total = Seen::default();
    for (n, (screen, seen)) in results.into_iter().enumerate() {
        let read: Read = control
            .call("read", json!({"session": sessions[n]}))
            .await
            .unwrap();
        assert_eq!(
            screen,
            read.text.trim_end_matches('\n'),
            "session {n}: {seen:?}"
        );
        assert!(seen.errors.is_empty(), "session {n}: {seen:?}");
        total.reconnects += seen.reconnects;
        total.resumes += seen.resumes;
        total.snapshots += seen.snapshots;
        total.gaps += seen.gaps;
    }
    // Six cuts over eight clients: reconnects happened, and most of them
    // resumed rather than bootstrapped again.
    assert!(total.reconnects >= 6, "{total:?}");
    assert!(total.resumes > 0, "{total:?}");
    assert_eq!(total.gaps, 0, "nothing fell behind the ring: {total:?}");
    eprintln!("loss run: {total:?}");
    drop(proxy);
    drop(server);
}

/// A server-side ring too small for what goes by while a client is kept
/// away: the resume is refused and the client bootstraps again (a refused
/// resume is a plain snapshot; `GAP` is for an attachment that falls
/// behind while attached).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_ring_turns_a_long_cut_into_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let registry = server::Registry::new(server::Config {
        limits: Limits {
            ring: 2048,
            ..Limits::default()
        },
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
        state_dir: dir.path().join("state"),
    })
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp = listener.local_addr().unwrap();
    let task = tokio::spawn(server::serve_tcp(listener, registry));
    let proxy = Proxy::start(tcp).await;
    let control = Endpoint::Tcp(tcp.to_string())
        .connect(hello("control", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap();
    // A session that runs what is typed, without a prompt; the control
    // connection types while the client is away.
    let session: Session = control
        .call(
            "create",
            json!({"argv": ["/bin/sh", "-c", "while read -r line; do eval \"$line\"; done"]}),
        )
        .await
        .unwrap();
    let type_line = |text: &str| {
        let data = base64::engine::general_purpose::STANDARD.encode(format!("{text}\n"));
        control.call::<_, serde_json::Value>("write", json!({"session": session.id, "data": data}))
    };
    type_line("echo hello").await.unwrap();
    let client = follow_thread(
        Endpoint::Tcp(proxy.addr.to_string()),
        session.id.clone(),
        "DONE".into(),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    proxy.paused.store(true, Ordering::Relaxed);
    proxy.cut();
    // More than a ring of output goes by while the client is kept away.
    type_line("i=0; while [ $i -lt 100 ]; do echo \"line $i ..............................\"; i=$((i+1)); done")
        .await
        .unwrap();
    loop {
        let read: Read = control
            .call("read", json!({"session": session.id}))
            .await
            .unwrap();
        if read.text.contains("line 99") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    proxy.paused.store(false, Ordering::Relaxed);
    // The typed command must not show the word the client waits for.
    type_line("printf 'DO%sNE\\n' ''").await.unwrap();
    let (screen, seen) = tokio::task::spawn_blocking(move || client.join().unwrap())
        .await
        .unwrap();
    let read: Read = control
        .call("read", json!({"session": session.id}))
        .await
        .unwrap();
    assert_eq!(screen, read.text.trim_end_matches('\n'), "{seen:?}");
    assert_eq!(
        (seen.reconnects, seen.snapshots, seen.resumes, seen.gaps),
        (1, 2, 0, 0),
        "{seen:?}"
    );
    task.abort();
    drop(proxy);
}
