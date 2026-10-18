//! Measurements recorded in MEASUREMENTS.md: bootstrap time and snapshot
//! size against scrollback, stream throughput per carrier, idle cost per
//! session, and a park. Prints a Markdown report.
//!
//!     cargo run --release -p clankermux --example measure

use std::path::Path;
use std::time::{Duration, Instant};

use client::connection::hello;
use client::{Connection, Endpoint};
use proto::*;
use serde_json::json;
use testkit::connect_ws;
use tokio_rustls::TlsConnector;

const MIB: u64 = 1024 * 1024;
/// A producer of `bytes` of distinct 80-column lines.
fn producer(bytes: u64) -> String {
    format!(
        "awk 'BEGIN {{ for (i = 0;; i++) printf \"%08d %s\\n\", i, \"{}\" }}' | head -c {bytes}",
        "x".repeat(70)
    )
}

struct Lab {
    server: testkit::Server,
    tcp: std::net::SocketAddr,
    wss: std::net::SocketAddr,
    connector: TlsConnector,
}

async fn lab() -> Lab {
    let server = testkit::Server::start(Limits::default());
    let tcp = server.listen_tcp().await;
    let (wss, connector) = server.listen_wss(vec!["http://measure".into()]).await;
    Lab {
        server,
        tcp,
        wss,
        connector,
    }
}

impl Lab {
    async fn connect(&self, carrier: &str) -> Connection {
        let hello = hello("measure", Some(ghostty_vt::ENGINE));
        match carrier {
            "unix" => Connection::connect_unix(&self.server.socket, hello)
                .await
                .unwrap(),
            "tcp" => Endpoint::Tcp(self.tcp.to_string())
                .connect(hello)
                .await
                .unwrap(),
            "wss" => {
                let ws = connect_ws(self.wss, &self.connector, "/", Some("http://measure"))
                    .await
                    .unwrap();
                Connection::handshake(
                    server::ws::WsStream::new(ws, Limits::default().max_frame),
                    hello,
                )
                .await
                .unwrap()
            }
            other => panic!("{other}"),
        }
    }
}

async fn create(conn: &Connection, script: &str) -> Session {
    conn.call(
        "create",
        json!({"argv": ["/bin/sh", "-c", script], "cols": 80, "rows": 24}),
    )
    .await
    .unwrap()
}

async fn wait_text(conn: &Connection, id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let read: Read = conn.call("read", json!({"session": id})).await.unwrap();
        if read.text.contains(needle) {
            return;
        }
        assert!(Instant::now() < deadline, "{needle} never appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn open(session: &str, resume: Option<Resume>) -> Open {
    Open {
        session: session.into(),
        read_only: false,
        viewport: Some(Size {
            cols: 80,
            rows: 24,
            cell_width: None,
            cell_height: None,
        }),
        profile: None,
        resume,
    }
}

/// OPEN to SNAPSHOT_END, and the snapshot's size.
async fn bootstrap(conn: &Connection, session: &str) -> (Duration, u64) {
    let started = Instant::now();
    let mut rx = conn.open(1, &open(session, None)).unwrap();
    let mut size = 0;
    loop {
        let frame = rx.recv().await.expect("a frame");
        match frame.header.frame_type {
            FrameType::Attached => {
                let attached: Attached = frame.json().unwrap();
                size = attached.snapshot_bytes.unwrap_or(0);
            }
            FrameType::SnapshotEnd => break,
            FrameType::Error => panic!("{}", String::from_utf8_lossy(&frame.payload)),
            _ => {}
        }
    }
    let elapsed = started.elapsed();
    conn.send(FrameType::Close, 1, &[]).unwrap();
    while let Some(frame) = rx.recv().await {
        if frame.header.frame_type == FrameType::Closed {
            break;
        }
    }
    (elapsed, size)
}

/// Bytes per second of OUTPUT from the first byte to the marker.
async fn throughput(lab: &Lab, carrier: &str, bytes: u64) -> (f64, usize) {
    let conn = lab.connect(carrier).await;
    let session = create(
        &conn,
        &format!(
            "read -r go; {}; echo; echo DONE-MARK; exec cat",
            producer(bytes)
        ),
    )
    .await;
    let mut rx = conn.open(1, &open(&session.id, None)).unwrap();
    loop {
        let frame = rx.recv().await.expect("a frame");
        if frame.header.frame_type == FrameType::SnapshotEnd {
            break;
        }
    }
    conn.send(FrameType::Input, 1, b"go\n").unwrap();
    let mut started = None;
    let mut received = 0u64;
    let mut gaps = 0;
    let mut tail: Vec<u8> = Vec::new();
    loop {
        let frame = rx.recv().await.expect("a frame");
        match frame.header.frame_type {
            FrameType::Output => {
                let (_, data) = parse_output(&frame.payload).unwrap();
                started.get_or_insert_with(Instant::now);
                received += data.len() as u64;
                tail.extend(data);
                let keep = tail.len().saturating_sub(32);
                tail.drain(..keep);
                if tail.windows(9).any(|w| w == b"DONE-MARK") {
                    break;
                }
            }
            FrameType::Gap => gaps += 1,
            FrameType::Error => panic!("{}", String::from_utf8_lossy(&frame.payload)),
            _ => {}
        }
    }
    let elapsed = started.unwrap().elapsed();
    conn.call::<_, serde_json::Value>("signal", json!({"session": session.id, "signal": "KILL"}))
        .await
        .unwrap();
    (received as f64 / elapsed.as_secs_f64() / MIB as f64, gaps)
}

fn ps(field: &str) -> String {
    let out = std::process::Command::new("ps")
        .args([
            "-o",
            &format!("{field}="),
            "-p",
            &std::process::id().to_string(),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn rss_mib() -> f64 {
    ps("rss").parse::<f64>().unwrap_or(0.0) / 1024.0
}

/// Cumulative CPU time from `ps -o time` (`[[h:]m:]s.cc`).
fn cpu_secs() -> f64 {
    ps("time").split(':').fold(0.0, |acc, part| {
        acc * 60.0 + part.parse::<f64>().unwrap_or(0.0)
    })
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| {
                    let path = e.path();
                    if path.is_dir() {
                        dir_size(&path)
                    } else {
                        e.metadata().map(|m| m.len()).unwrap_or(0)
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// macOS only; empty elsewhere.
fn sysctl(name: &str) -> String {
    std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[tokio::main]
async fn main() {
    let lab = lab().await;
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("# Measurements\n");
    println!(
        "Commit `{commit}`, {} ({}, {} GiB), {}, `cargo run --release -p clankermux --example measure`.\n",
        sysctl("machdep.cpu.brand_string"),
        sysctl("hw.machine"),
        sysctl("hw.memsize").parse::<u64>().unwrap_or(0) / (1024 * MIB),
        std::process::Command::new("date")
            .arg("+%Y-%m-%d")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    );
    println!("Default limits; the server in-process; clients in the same process over loopback.\n");

    let control = lab.connect("unix").await;
    println!("## Idle cost\n");
    println!("| Idle sessions | RSS | CPU over 10 s |");
    println!("| --- | --- | --- |");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let base = rss_mib();
    println!("| 0 | {base:.1} MiB | |");
    let mut ids = Vec::new();
    for target in [1usize, 10, 50] {
        while ids.len() < target {
            let session: Session = control
                .call("create", json!({"cols": 80, "rows": 24}))
                .await
                .unwrap();
            ids.push(session.id);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let cpu_before = cpu_secs();
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(10)).await;
        let cpu = (cpu_secs() - cpu_before) / started.elapsed().as_secs_f64() * 100.0;
        println!("| {target} | {:.1} MiB | {cpu:.1}% |", rss_mib());
    }
    for id in ids {
        control
            .call::<_, serde_json::Value>("signal", json!({"session": id, "signal": "KILL"}))
            .await
            .unwrap();
    }

    println!("\n## Bootstrap\n");
    println!("| Scrollback written | Rows retained | Snapshot bytes | OPEN to SNAPSHOT_END |");
    println!("| --- | --- | --- | --- |");
    let mut big = None;
    for mib in [0u64, 1, 4, 10] {
        let session = create(
            &control,
            &format!("{}; echo; echo READY-MARK; exec cat", producer(mib * MIB)),
        )
        .await;
        wait_text(&control, &session.id, "READY-MARK").await;
        let conn = lab.connect("unix").await;
        let (elapsed, size) = bootstrap(&conn, &session.id).await;
        let rows = match &lab
            .server
            .registry
            .get(&session.id)
            .unwrap()
            .lock()
            .unwrap()
            .body
        {
            server::session::Body::Live(live) => live.term.scrollback_rows(),
            server::session::Body::Parked(_) => 0,
        };
        println!(
            "| {mib} MiB | {rows} | {:.2} MiB | {:.0} ms |",
            size as f64 / MIB as f64,
            elapsed.as_secs_f64() * 1000.0
        );
        if mib == 10 {
            big = Some(session.id);
        } else {
            control
                .call::<_, serde_json::Value>(
                    "signal",
                    json!({"session": session.id, "signal": "KILL"}),
                )
                .await
                .unwrap();
        }
    }

    println!("\n## Park\n");
    let big = big.unwrap();
    let shared = lab.server.registry.get(&big).unwrap();
    let started = Instant::now();
    lab.server.registry.park(&shared).await;
    let elapsed = started.elapsed();
    let size = dir_size(&lab.server.dir.path().join("state/sessions").join(&big));
    println!(
        "A session with 10 MiB of scrollback parks in {:.0} ms into {:.2} MiB of files.\n",
        elapsed.as_secs_f64() * 1000.0,
        size as f64 / MIB as f64
    );
    control
        .call::<_, serde_json::Value>("signal", json!({"session": big, "signal": "KILL"}))
        .await
        .unwrap();

    println!("## Stream throughput\n");
    println!("64 MiB of 80-column lines from the PTY to one attached client.\n");
    println!("| Carrier | MiB/s | Gaps |");
    println!("| --- | --- | --- |");
    for carrier in ["unix", "tcp", "wss"] {
        let (rate, gaps) = throughput(&lab, carrier, 64 * MIB).await;
        println!("| {carrier} | {rate:.0} | {gaps} |");
    }

    // The lab (its server and temporary directory) goes before the exit,
    // which skips `Drop`; the exit itself is for the killed sessions'
    // reader tasks, which a runtime drop would wait for.
    drop(lab);
    std::process::exit(0);
}
