//! The binary against an in-process server, driven through a PTY.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use client::{Connection, connection::hello};
use proto::*;
use serde_json::json;
use tokio::io::unix::AsyncFd;
use tokio::process::Child;

use testkit::Server;

fn start() -> Server {
    Server::start(Limits::default())
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

/// Runs the binary with `args` outside any session (no inherited
/// `CLANKERMUX_*`) plus `env`; its exit code, stdout and stderr.
async fn run_status(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"));
    command
        .args(args)
        .env_remove("CLANKERMUX_SESSION")
        .env_remove("CLANKERMUX_SOCKET")
        .envs(env.iter().copied());
    let output = command.output().await.unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
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
        tokio::time::timeout(Duration::from_secs(10), self.child.wait())
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
    let server = start();
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
    let server = start();
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
    let mut server = start();
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"], "cols": 80, "rows": 24})).await;
    let mut term = Term::attach(&server, 80, 24, &[&session.id]);
    term.read_until(b"\x1b[?25h").await;
    drop(conn);
    // The server goes away with its connections; the attach is
    // reconnecting in the background, and the prefix still works.
    server.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    term.detach().await;
    assert_eq!(term.exit_code().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_retries_a_bare_close_and_ends_on_a_refusal() {
    let mut server = start();
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"], "cols": 80, "rows": 24})).await;
    let mut term = Term::attach(&server, 80, 24, &[&session.id]);
    term.read_until(b"\x1b[?25h").await;
    drop(conn);
    server.shutdown();
    // A stand-in on the same path: as a forwarder would, it accepts and
    // drops two connections, then answers HELLO with a refusal.
    let _ = std::fs::remove_file(&server.socket);
    let listener = tokio::net::UnixListener::bind(&server.socket).unwrap();
    for _ in 0..2 {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    }
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
        .await
        .unwrap();
    assert!(n > 0, "HELLO expected");
    let refusal = encode_frame(
        FrameType::Error,
        0,
        br#"{"code":"version_mismatch","message":"the stand-in speaks version 9"}"#,
    );
    tokio::io::AsyncWriteExt::write_all(&mut stream, &refusal)
        .await
        .unwrap();
    drop(stream);
    let out = term.read_until(b"version 9").await;
    assert!(
        String::from_utf8_lossy(&out).contains("version_mismatch"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(term.exit_code().await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn attaching_to_an_exited_session_reports_its_exit() {
    let server = start();
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
    let server = start();
    let conn = connect(&server).await;
    let session = create(&conn, json!({"label": "over-tcp", "argv": ["/bin/cat"]})).await;
    let addr = server.listen_tcp().await.to_string();
    let listed = run(&["--connect", &addr, "list"], &[]).await;
    assert!(listed.contains("over-tcp"), "{listed}");
    let read = run(&["--connect", &addr, "--json", "read", &session.id], &[]).await;
    assert!(read.contains("\"status\": \"running\""), "{read}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ssh_relays_through_stdio() {
    let server = start();
    let conn = connect(&server).await;
    create(&conn, json!({"label": "over-ssh", "argv": ["/bin/cat"]})).await;
    // A stand-in `ssh` that runs the relay against the test socket, as
    // the real one would run it on the remote host.
    let bin = server.dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(
        bin.join("ssh"),
        format!(
            "#!/bin/sh\ntest \"$1 $2 $3\" = 'remote-host clankermux --stdio' || exit 9\n\
             exec {} --socket {} --stdio\n",
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

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_ends_when_the_server_side_does() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("sock");
    // A stand-in server that accepts one connection and drops it.
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
        .args(["--socket", socket.to_str().unwrap(), "--stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    drop(stream);
    // stdin stays open: the relay must not wait for its EOF.
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("the relay exits")
        .unwrap();
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn websocket_flags_need_the_websocket() {
    // Isolated, so that a regression of the check cannot start a server on
    // the user's own socket and state directory.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("sock");
    for args in [
        ["--origin", "http://page.test"],
        ["--tls-cert", "c"],
        ["--tls-key", "k"],
    ] {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
            .arg("--socket")
            .arg(&socket)
            .arg("serve")
            .args(args)
            .env("XDG_STATE_HOME", dir.path())
            .output()
            .await
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains("--ws"),
            "{stderr}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn send_chunks_to_the_input_queue() {
    let server = Server::start(Limits {
        input_queue: 16,
        ..Limits::default()
    });
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"]})).await;
    let text = "0123456789abcdefghijklmnopqrstuvwxyz\n";
    run(
        &[
            "--socket",
            server.socket.to_str().unwrap(),
            "send",
            &session.id,
            text,
        ],
        &[],
    )
    .await;
    wait_session(&conn, &session.id, |s| s.offset >= 2 * text.len() as u64).await;
    let read: Read = conn
        .call("read", json!({"session": session.id}))
        .await
        .unwrap();
    assert!(read.text.contains(text), "{}", read.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn send_sends_one_request_even_for_empty_text() {
    let server = start();
    let conn = connect(&server).await;
    let session = create(
        &conn,
        json!({"argv": ["/bin/sh", "-c", "exit 0"], "on_exit": "keep"}),
    )
    .await;
    wait_session(&conn, &session.id, |s| s.status == Status::Exited).await;
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
        .args([
            "--socket",
            server.socket.to_str().unwrap(),
            "send",
            &session.id,
            "",
        ])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success(), "an exited session refuses a send");
    assert!(String::from_utf8_lossy(&output.stderr).contains("not_running"));
}

/// `clankermux serve` as a process on `dir`'s socket and state, parking
/// soon after a change.
fn serve(dir: &std::path::Path) -> Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
        .arg("--socket")
        .arg(dir.join("sock"))
        .args(["serve", "--park-timing", "3600,0.2,0.2,1"])
        .env("XDG_STATE_HOME", dir.join("state"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Connects once the server at `dir` answers; a process start can take
/// seconds on a loaded machine.
async fn connect_when_up(dir: &std::path::Path) -> Connection {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match Connection::connect_unix(&dir.join("sock"), hello("test", Some(ghostty_vt::ENGINE)))
            .await
        {
            Ok(conn) => return conn,
            Err(err) => assert!(tokio::time::Instant::now() < deadline, "{err}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_server_restores_what_was_parked_after_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = serve(dir.path());
    let conn = connect_when_up(dir.path()).await;
    let session = create(&conn, json!({"argv": ["/bin/sh", "-c", "exec cat"]})).await;
    conn.request(
        "send",
        json!({"session": session.id, "data": "YmVmb3JlLXRoZS1raWxsCg=="}),
    )
    .await
    .unwrap();
    // A park holding the line, not merely one from before the write.
    let parks = dir
        .path()
        .join("state/clankermux/sessions")
        .join(&session.id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let parked = std::fs::read_dir(&parks)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .any(|e| {
                std::fs::read_to_string(e.path().join("screen.txt"))
                    .is_ok_and(|t| t.contains("before-the-kill"))
            });
        if parked {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never parked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(conn);
    // SIGKILL: no stop, no park on the way down, as at a power-off.
    first.kill().await.unwrap();

    let _second = serve(dir.path());
    let conn = connect_when_up(dir.path()).await;
    let restored = inspect(&conn, &session.id).await;
    assert_eq!(restored.status, Status::Restored);
    let read: Read = conn
        .call("read", json!({"session": session.id}))
        .await
        .unwrap();
    assert!(read.text.contains("before-the-kill"), "{}", read.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_argument_resolves_by_id_label_and_either_end() {
    let server = start();
    let conn = connect(&server).await;
    let alpha = create(&conn, json!({"label": "Alpha", "argv": ["/bin/cat"]})).await;
    let twins = [
        create(&conn, json!({"label": "twin", "argv": ["/bin/cat"]})).await,
        create(&conn, json!({"label": "TWIN", "argv": ["/bin/cat"]})).await,
    ];
    let socket = server.socket.to_str().unwrap();
    let label_of = |needle: &str| {
        let needle = needle.to_string();
        async move {
            let (code, out, err) =
                run_status(&["--socket", socket, "--json", "inspect", &needle], &[]).await;
            assert_eq!(code, 0, "{needle}: {err}");
            serde_json::from_str::<Session>(&out).unwrap().label
        }
    };
    assert_eq!(label_of(&alpha.id).await, "Alpha");
    assert_eq!(label_of("alpha").await, "Alpha");
    assert_eq!(label_of(&alpha.id[..8]).await, "Alpha");
    assert_eq!(label_of(&alpha.id[alpha.id.len() - 6..]).await, "Alpha");

    // Several labels match regardless of case: exit 3, the candidates
    // on stderr by their last six characters.
    let (code, _, err) = run_status(&["--socket", socket, "inspect", "Twin"], &[]).await;
    assert_eq!(code, 3, "{err}");
    for twin in &twins {
        assert!(err.contains(&twin.id[twin.id.len() - 6..]), "{err}");
    }
    assert!(!err.contains(&alpha.id[alpha.id.len() - 6..]), "{err}");
    // None: every session is a candidate. Three characters are too few.
    let (code, _, err) = run_status(&["--socket", socket, "end", "nosuch"], &[]).await;
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("Alpha") && err.contains("running"), "{err}");
    let (code, _, err) = run_status(&["--socket", socket, "end", &alpha.id[..3]], &[]).await;
    assert_eq!(code, 3, "{err}");

    // Left out: `$CLANKERMUX_SESSION` of the server in
    // `$CLANKERMUX_SOCKET`, else a usage error.
    let (code, out, err) = run_status(
        &["--socket", socket, "--json", "inspect"],
        &[
            ("CLANKERMUX_SESSION", &alpha.id),
            ("CLANKERMUX_SOCKET", socket),
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Alpha"), "{out}");
    let (code, _, err) = run_status(&["--socket", socket, "inspect"], &[]).await;
    assert_eq!(code, 2, "{err}");
    // `label` takes the session from there too, and the socket.
    let (code, _, err) = run_status(
        &["label", "renamed"],
        &[
            ("CLANKERMUX_SESSION", &alpha.id),
            ("CLANKERMUX_SOCKET", socket),
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(inspect(&conn, &alpha.id).await.label, "renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_codes_tell_failure_usage_target_and_server_apart() {
    let server = start();
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"]})).await;
    let socket = server.socket.to_str().unwrap();
    // The server refused: 1.
    let (code, _, err) = run_status(&["--socket", socket, "remove", &session.id], &[]).await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("running"), "{err}");
    // Usage, clap's and ours: 2.
    let (code, _, _) = run_status(&["--socket", socket, "resize"], &[]).await;
    assert_eq!(code, 2);
    let (code, _, err) =
        run_status(&["--socket", socket, "resize", &session.id, "wide"], &[]).await;
    assert_eq!(code, 2, "{err}");
    // No such session: 3.
    let (code, _, err) = run_status(&["--socket", socket, "read", "nothing-here"], &[]).await;
    assert_eq!(code, 3, "{err}");
    // No server: 4.
    let dead = server.dir.path().join("no-server");
    let (code, _, err) = run_status(&["--socket", dead.to_str().unwrap(), "list"], &[]).await;
    assert_eq!(code, 4, "{err}");
    // `resize` keeps the side left out.
    let (code, _, err) = run_status(&["--socket", socket, "resize", &session.id, "x30"], &[]).await;
    assert_eq!(code, 0, "{err}");
    let resized = wait_session(&conn, &session.id, |s| s.rows == 30).await;
    assert_eq!(resized.cols, 80);
    let (code, out, _) = run_status(&["--socket", socket, "list"], &[]).await;
    assert_eq!(code, 0);
    assert!(
        out.starts_with(&session.id[session.id.len() - 6..]),
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_is_checked_first_and_the_session_default_is_its_server_s() {
    let server = start();
    let conn = connect(&server).await;
    let session = create(&conn, json!({"argv": ["/bin/cat"]})).await;
    conn.call::<_, serde_json::Value>("label", json!({"session": session.id, "label": "build"}))
        .await
        .unwrap();
    let socket = server.socket.to_str().unwrap();
    // Usage before the connection: 2, server or not.
    let dead = server.dir.path().join("no-server");
    let dead = dead.to_str().unwrap();
    for args in [
        &["resize", "build", "wide"][..],
        &["wait", "build"],
        &["send", "build", "--keys", "esc", "nokey"],
        &["send", "build", "text", "--bytes"],
    ] {
        let (code, _, err) = run_status(&[&["--socket", dead], args].concat(), &[]).await;
        assert_eq!(code, 2, "{args:?}: {err}");
    }
    // Inside the session, one argument is TEXT, unless it names a session.
    let inside = [
        ("CLANKERMUX_SESSION", session.id.as_str()),
        ("CLANKERMUX_SOCKET", socket),
    ];
    let (code, _, err) = run_status(&["send", "build", "--enter"], &inside).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("names a session"), "{err}");
    let (code, _, err) = run_status(&["send", "hello", "--enter"], &inside).await;
    assert_eq!(code, 0, "{err}");
    // `$CLANKERMUX_SESSION` is not a session of another server.
    let elsewhere = [
        ("CLANKERMUX_SESSION", session.id.as_str()),
        ("CLANKERMUX_SOCKET", dead),
    ];
    let (code, _, err) = run_status(&["--socket", socket, "inspect"], &elsewhere).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not of this one"), "{err}");
    let (code, _, err) = run_status(&["--socket", socket, "inspect"], &inside).await;
    assert_eq!(code, 0, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_inside_a_session_reaches_its_server_and_session() {
    // A relative socket: the child, elsewhere, still finds it.
    let dir = tempfile::tempdir().unwrap();
    let _server = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
        .args(["--socket", "sock", "serve"])
        .current_dir(dir.path())
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env_remove("CLANKERMUX_SOCKET")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let conn = connect_when_up(dir.path()).await;
    let script = format!(
        "{} label from-inside && printf '<%s>' \"$CLANKERMUX_SOCKET\"; exec cat",
        env!("CARGO_BIN_EXE_clankermux")
    );
    let session = create(
        &conn,
        json!({"argv": ["/bin/sh", "-c", script], "cwd": "/"}),
    )
    .await;
    let labelled = wait_session(&conn, &session.id, |s| s.label == "from-inside").await;
    assert_eq!(labelled.id, session.id);
    let socket = std::fs::canonicalize(dir.path().join("sock")).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let read: Read = conn
            .call("read", json!({"session": session.id}))
            .await
            .unwrap();
        let shown = read.text.replace('\n', "");
        let canonical = shown
            .split_once('<')
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(path, _)| std::fs::canonicalize(path).unwrap());
        if let Some(path) = canonical {
            assert_eq!(path, socket, "{shown}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{shown}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_guide_is_printed_without_a_server() {
    let dead = tempfile::tempdir().unwrap();
    let socket = dead.path().join("sock");
    let (code, shown, _) = run_status(
        &["--socket", socket.to_str().unwrap(), "skill", "show"],
        &[],
    )
    .await;
    assert_eq!(code, 0);
    let (code, flag, _) = run_status(&["--skill"], &[]).await;
    assert_eq!((code, &flag), (0, &shown));
    for needle in ["CLANKERMUX_SESSION", "CLANKERMUX_SOCKET", "--json", "| 3 |"] {
        assert!(shown.contains(needle), "{needle}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn waits_exit_with_the_child_status_a_timeout_or_an_interrupt() {
    let server = start();
    let conn = connect(&server).await;
    let socket = server.socket.to_str().unwrap();
    // create --wait: the child's code, however fast it exits.
    let (code, out, err) = run_status(
        &[
            "--socket", socket, "create", "--wait", "--", "/bin/sh", "-c", "exit 5",
        ],
        &[],
    )
    .await;
    assert_eq!(code, 5, "{err}");
    assert_eq!(out, "exited 5\n");

    // A signal: 128 + its number.
    let session = create(&conn, json!({"argv": ["/bin/cat"], "on_exit": "keep"})).await;
    let waiting = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
        .args(["--socket", socket, "wait", &session.id, "--wait-exit"])
        .output();
    let killer = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        conn.request("signal", json!({"session": session.id, "signal": "KILL"}))
            .await
            .unwrap();
    };
    let (output, ()) = tokio::join!(waiting, killer);
    let output = output.unwrap();
    assert_eq!(output.status.code(), Some(128 + 9), "{output:?}");

    // A timeout: 124. Text and keys with a match.
    let session = create(&conn, json!({"argv": ["/bin/cat"]})).await;
    let (code, out, _) = run_status(
        &[
            "--socket",
            socket,
            "wait",
            &session.id,
            "--match",
            "never",
            "--timeout",
            "200ms",
        ],
        &[],
    )
    .await;
    assert_eq!((code, out.as_str()), (124, "timeout at 0\n"));
    let (code, out, err) = run_status(
        &[
            "--socket",
            socket,
            "send",
            &session.id,
            "polo",
            "--enter",
            "--wait-match",
            "po+lo",
        ],
        &[],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "matched \"polo\" at 0\n");
    let (code, out, err) = run_status(
        &[
            "--socket",
            socket,
            "--json",
            "send",
            &session.id,
            "--keys",
            "x",
            "enter",
            "--wait-match",
            "x",
        ],
        &[],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let sent: Sent = serde_json::from_str(&out).unwrap();
    assert_eq!(sent.wait.unwrap().reason, WaitReason::Matched);
    // Usage: nothing to wait for; a timeout alone.
    let (code, _, _) = run_status(&["--socket", socket, "wait", &session.id], &[]).await;
    assert_eq!(code, 2);
    let (code, _, _) = run_status(
        &[
            "--socket",
            socket,
            "send",
            &session.id,
            "x",
            "--timeout",
            "1s",
        ],
        &[],
    )
    .await;
    assert_eq!(code, 2);

    // Ctrl-C cancels the wait: 130.
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_clankermux"))
        .args(["--socket", socket, "wait", &session.id, "--exit"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Long enough for the binary to start and listen for Ctrl-C on a
    // loaded machine; before that, SIGINT would kill it outright.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pid = rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("the wait ends")
        .unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cancelled"));
}
