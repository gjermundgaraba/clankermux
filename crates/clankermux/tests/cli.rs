//! The binary against an in-process server, driven through a PTY: attach
//! to a `cat` session, type, see the echo, detach with the prefix.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use client::{Connection, connection::hello};
use proto::*;
use serde_json::json;
use tokio::io::unix::AsyncFd;

struct Server {
    socket: PathBuf,
    dir: tempfile::TempDir,
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
    });
    let task = tokio::spawn(server::serve_unix(socket.clone(), registry));
    // The socket file exists before the server listens on it.
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Server { socket, dir, task }
}

/// The outer terminal the binary runs in.
struct Term {
    master: AsyncFd<OwnedFd>,
    seen: Vec<u8>,
}

impl Term {
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
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_types_and_detaches() {
    let server = start().await;
    let conn = Connection::connect_unix(&server.socket, hello("test", Some(ghostty_vt::ENGINE)))
        .await
        .unwrap();
    let session: Session = conn
        .call(
            "create",
            json!({"argv": ["/bin/cat"], "cols": 80, "rows": 24}),
        )
        .await
        .unwrap();

    let argv: Vec<String> = [
        env!("CARGO_BIN_EXE_clankermux"),
        "attach",
        &session.id,
        "--socket",
        server.socket.to_str().unwrap(),
    ]
    .map(String::from)
    .into();
    let (pty, mut child) =
        server::pty::spawn(&argv, server.dir.path(), &BTreeMap::new(), 80, 24).unwrap();
    // SAFETY: the master is an owned, non-blocking fd registered once.
    let master = unsafe { AsyncFd::register(pty.master) }.unwrap();
    let mut term = Term {
        master,
        seen: Vec::new(),
    };
    // The first paint ends by showing the cursor.
    term.read_until(b"\x1b[?25h").await;

    // Raw mode: the outer terminal echoes nothing; the session does.
    term.write(b"hello\n");
    term.read_until(b"hello\r\nhello\r\n").await;

    term.write(b"\x1c");
    term.write(b"d");
    let out = term.read_until(b"[clankermux: detached]").await;
    assert!(
        out.windows(RESET_TAIL.len()).any(|w| w == RESET_TAIL),
        "the outer terminal is reset before the message"
    );
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status:?}");

    let inspected: Session = conn
        .call("inspect", json!({"session": session.id}))
        .await
        .unwrap();
    assert_eq!(inspected.attachments, 0);
    assert_eq!((inspected.cols, inspected.rows), (80, 24));
}

/// The end of the reset sequence the attach writes on exit.
const RESET_TAIL: &[u8] = b"\x1b[<u";
