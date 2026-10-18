//! The binary against an in-process server, driven through a PTY.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use client::{Connection, connection::hello};
use proto::*;
use serde_json::json;
use tokio::io::unix::AsyncFd;
use tokio::process::Child;

struct Server {
    socket: PathBuf,
    dir: tempfile::TempDir,
    registry: std::sync::Arc<server::Registry>,
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
        limits: Limits::default(),
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
        state_dir: dir.path().join("state"),
    })
    .unwrap();
    let task = tokio::spawn(server::serve_unix(socket.clone(), registry.clone()));
    // The socket file exists before the server listens on it.
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Server {
        socket,
        dir,
        registry,
        task,
    }
}

/// Runs the binary with `args` and returns its stdout; the process must
/// succeed.
async fn run(args: &[&str], env: &[(&str, String)]) -> String {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"));
    command.args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn connect(server: &Server) -> Connection {
    Connection::connect_unix(&server.socket, hello("test", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap()
}

async fn create(conn: &Connection, params: serde_json::Value) -> Session {
    conn.call("create", params).await.unwrap()
}

async fn inspect(conn: &Connection, id: &str) -> Session {
    conn.call("inspect", json!({"session": id})).await.unwrap()
}

/// Polls `inspect` until `ok` holds.
async fn wait_session(conn: &Connection, id: &str, ok: impl Fn(&Session) -> bool) -> Session {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let session = inspect(conn, id).await;
        if ok(&session) {
            return session;
        }
        assert!(tokio::time::Instant::now() < deadline, "{session:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The outer terminal the binary runs in.
struct Term {
    master: AsyncFd<OwnedFd>,
    seen: Vec<u8>,
    child: Child,
}

impl Term {
    /// Runs `clankermux attach` with `args` in a `cols` by `rows` PTY.
    fn attach(server: &Server, cols: u16, rows: u16, args: &[&str]) -> Term {
        let mut argv = vec![
            env!("CARGO_BIN_EXE_clankermux").to_string(),
            "--socket".to_string(),
            server.socket.to_str().unwrap().to_string(),
            "attach".to_string(),
        ];
        argv.extend(args.iter().map(|s| s.to_string()));
        let (pty, child) =
            server::pty::spawn(&argv, server.dir.path(), &BTreeMap::new(), cols, rows).unwrap();
        // SAFETY: the master is an owned, non-blocking fd registered once.
        let master = unsafe { AsyncFd::register(pty.master) }.unwrap();
        Term {
            master,
            seen: Vec::new(),
            child,
        }
    }

    /// Reads until `needle` has appeared; returns everything read so far.
    async fn read_until(&mut self, needle: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.seen.windows(needle.len()).any(|w| w == needle) {
                return std::mem::take(&mut self.seen);
            }
            let mut guard = tokio::time::timeout_at(deadline, self.master.readable())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "{:?} not seen in {:?}",
                        String::from_utf8_lossy(needle),
                        String::from_utf8_lossy(&self.seen)
                    )
                })
                .unwrap();
            match guard
                .try_io(|fd| rustix::io::read(fd.get_ref(), &mut buf).map_err(std::io::Error::from))
            {
                Ok(Ok(0)) | Ok(Err(_)) => panic!(
                    "the terminal closed before {:?}: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&self.seen)
                ),
                Ok(Ok(n)) => self.seen.extend_from_slice(&buf[..n]),
                Err(_would_block) => continue,
            }
        }
    }

    fn write(&self, bytes: &[u8]) {
        let n = rustix::io::write(self.master.get_ref(), bytes).unwrap();
        assert_eq!(n, bytes.len());
    }

    /// Detaches with the prefix and returns the output up to the exit
    /// message, checking the reset came before it.
    async fn detach(&mut self) -> Vec<u8> {
        self.write(b"\x1c");
        self.write(b"d");
        let out = self.read_until(b"[clankermux: detached]").await;
        assert!(
            out.windows(RESET_TAIL.len()).any(|w| w == RESET_TAIL),
            "the outer terminal is reset before the message"
        );
        out
    }

    async fn exit_code(&mut self) -> i32 {
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .unwrap()
            .unwrap()
            .code()
            .unwrap()
    }
}

/// The end of the reset sequence the attach writes on exit.
const RESET_TAIL: &[u8] = b"\x1b[?2026l\x1b[?25h";
/// The start of a paint.
const CLEAR: &[u8] = b"\x1b[H\x1b[2J";

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_types_and_detaches() {
    let server = start().await;
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"], "cols": 80, "rows": 24})).await;
    let mut term = Term::attach(&server, 80, 24, &[&session.id, "--detach-key", "C-a"]);
    // The first paint ends by showing the cursor.
    term.read_until(b"\x1b[?25h").await;

    // Raw mode: the outer terminal echoes nothing; the session does.
    term.write(b"hello\n");
    term.read_until(b"hello\r\nhello\r\n").await;
    // The prefix twice sends it once; the session's cooked mode echoes it.
    term.write(b"\x01\x01\n");
    term.read_until(b"^A\r\n").await;

    term.write(b"\x01d");
    let out = term.read_until(b"[clankermux: detached]").await;
    assert!(out.windows(RESET_TAIL.len()).any(|w| w == RESET_TAIL));
    assert_eq!(term.exit_code().await, 0);
    let inspected = wait_session(&conn, &session.id, |s| s.attachments == 0).await;
    assert_eq!((inspected.cols, inspected.rows), (80, 24));
}

#[tokio::test(flavor = "multi_thread")]
async fn repaint_clips_a_larger_session_to_the_bottom() {
    let server = start().await;
    let conn = connect(&server).await;
    let session = create(
        &conn,
        json!({"argv": ["/usr/bin/yes"], "cols": 100, "rows": 40}),
    )
    .await;
    let mut term = Term::attach(&server, 80, 24, &[&session.id, "--read-only"]);
    // With `yes` at the bottom row the window is the last 24 rows and the
    // cursor lands on the outer's last row.
    let out = term.read_until(b"\x1b[24;1H").await;
    assert!(
        count(&out, CLEAR) >= 1,
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    // Output keeps coming and is repainted, not passed through.
    let out = term.read_until(b"\x1b[24;1H").await;
    let paints = count(&out, CLEAR);
    assert!(paints >= 1);
    // Each paint carries at most the 24 clipped rows; passthrough would
    // carry everything `yes` wrote meanwhile.
    assert!(
        count(&out, b"y\r\n") <= 24 * paints,
        "{:?}",
        String::from_utf8_lossy(&out)
    );

    term.detach().await;
    assert_eq!(term.exit_code().await, 0);
    let inspected = wait_session(&conn, &session.id, |s| s.attachments == 0).await;
    assert_eq!((inspected.cols, inspected.rows), (100, 40), "read-only");
}

#[tokio::test(flavor = "multi_thread")]
async fn detaches_while_the_server_is_gone() {
    let server = start().await;
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"], "cols": 80, "rows": 24})).await;
    let mut term = Term::attach(&server, 80, 24, &[&session.id]);
    term.read_until(b"\x1b[?25h").await;
    drop(conn);
    drop(server);
    // Reconnecting in the background; the prefix still works.
    tokio::time::sleep(Duration::from_millis(300)).await;
    term.detach().await;
    assert_eq!(term.exit_code().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn attaching_to_an_exited_session_reports_its_exit() {
    let server = start().await;
    let conn = connect(&server).await;
    let session = create(
        &conn,
        json!({"argv": ["/bin/sh", "-c", "echo bye; exit 3"], "on_exit": "keep"}),
    )
    .await;
    wait_session(&conn, &session.id, |s| s.status == Status::Exited).await;
    let mut term = Term::attach(&server, 80, 24, &[&session.id]);
    let out = term
        .read_until(b"[clankermux: session exited with code 3]")
        .await;
    assert!(
        count(&out, b"bye") >= 1,
        "the final screen is painted first"
    );
    assert_eq!(term.exit_code().await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_reaches_a_tcp_carrier() {
    let server = start().await;
    let conn = connect(&server).await;
    let session = create(&conn, json!({"label": "over-tcp", "argv": ["/bin/cat"]})).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(server::serve_tcp(listener, server.registry.clone()));
    let listed = run(&["--connect", &addr, "list"], &[]).await;
    assert!(listed.contains("over-tcp"), "{listed}");
    let read = run(&["--connect", &addr, "--json", "read", &session.id], &[]).await;
    assert!(read.contains("\"status\": \"running\""), "{read}");
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn ssh_relays_through_stdio() {
    let server = start().await;
    let conn = connect(&server).await;
    create(&conn, json!({"label": "over-ssh", "argv": ["/bin/cat"]})).await;
    // A stand-in `ssh` that runs the relay against the test socket, as
    // the real one would run it on the remote host.
    let bin = server.dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(
        bin.join("ssh"),
        format!(
            "#!/bin/sh\ntest \"$1\" = remote-host || exit 9\nexec {} --socket {} --stdio\n",
            env!("CARGO_BIN_EXE_clankermux"),
            server.socket.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let listed = run(&["--ssh", "remote-host", "list"], &[("PATH", path)]).await;
    assert!(listed.contains("over-ssh"), "{listed}");
}
