//! Park files, restart, and program status records.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use proto::*;
use serde_json::json;

/// The milliseconds between consecutive generations, by their names.
fn spacing(parked: &[PathBuf]) -> Vec<u64> {
    let ms: Vec<u64> = parked
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().parse().unwrap())
        .collect();
    ms.windows(2).map(|w| w[1] - w[0]).collect()
}

/// Writes `tick-N` lines to `id` every `every` for at most `max`,
/// collecting each new newest generation until there are `want`; also
/// the longest time between two writes.
async fn tick(
    c: &mut Client,
    state: &Path,
    id: &str,
    every: Duration,
    max: Duration,
    want: usize,
) -> (Vec<PathBuf>, Duration) {
    let mut parked = Vec::new();
    let mut longest = Duration::ZERO;
    let start = tokio::time::Instant::now();
    let mut previous = start;
    for tick in 0.. {
        c.ok(
            "send",
            json!({"session": id, "data": b64(format!("tick-{tick}\n"))}),
        )
        .await;
        let now = tokio::time::Instant::now();
        longest = longest.max(now - previous);
        previous = now;
        if let Some(newest) = generations(state, id).pop()
            && parked.last() != Some(&newest)
        {
            parked.push(newest);
        }
        if parked.len() == want || now - start > max {
            break;
        }
        tokio::time::sleep(every).await;
    }
    (parked, longest)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_parks_once_quiet_and_while_busy() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let server =
        Server::start_with_park(fast_limits(), state.clone(), prompt_parks(400, 200, 1000));
    let mut c = client(&server).await;

    // Input echoed once, then quiet.
    let quiet = c
        .create(json!({"argv": ["/bin/sh", "-c", "exec cat"]}))
        .await;
    c.ok(
        "send",
        json!({"session": quiet.id, "data": b64("typed-line\n")}),
    )
    .await;
    wait_parked(&state, &quiet.id, "screen.txt", |t| {
        t.contains("typed-line")
    })
    .await;

    // A line every 50 ms is never quiet for 400 ms: it parks every
    // second all the same, and no more often. The test writes the lines itself:
    // a loop of processes would load the machine the other tests share.
    let busy = c
        .create(json!({"argv": ["/bin/sh", "-c", "exec cat"]}))
        .await;
    let (parked, longest) = tick(
        &mut c,
        &state,
        &busy.id,
        Duration::from_millis(50),
        Duration::from_secs(10),
        3,
    )
    .await;
    if longest >= Duration::from_millis(400) {
        // The machine let the session go quiet: not this test's case.
        eprintln!("skipped: {longest:?} between two writes");
        return;
    }
    assert_eq!(parked.len(), 3, "{parked:?}");
    assert!(
        spacing(&parked).iter().all(|ms| *ms >= 900),
        "{:?}",
        spacing(&parked)
    );
    assert_eq!(c.inspect(&busy.id).await.status, Status::Running);
}

#[tokio::test(flavor = "multi_thread")]
async fn bursts_park_no_more_often_than_the_gap() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let server =
        Server::start_with_park(fast_limits(), state.clone(), prompt_parks(100, 800, 5000));
    let mut c = client(&server).await;
    // A line every 250 ms is quiet for 100 ms each time; without the
    // gap it would park four times a second.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "exec cat"]}))
        .await;
    let (parked, _) = tick(
        &mut c,
        &state,
        &session.id,
        Duration::from_millis(250),
        Duration::from_millis(2500),
        usize::MAX,
    )
    .await;
    assert!(parked.len() >= 2, "{parked:?}");
    assert!(
        spacing(&parked).iter().all(|ms| *ms >= 750),
        "{:?}",
        spacing(&parked)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_detach_parks() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let server = Server::start_with_park(
        fast_limits(),
        state.clone(),
        prompt_parks(60_000, 60_000, 60_000),
    );
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "exec cat"]}))
        .await;
    c.open(1, json!({"session": session.id})).await;
    c.bootstrap(1).await;
    c.send(FrameType::Input, 1, b"detached-line\n").await;
    c.output_until(1, b"detached-line").await;
    assert!(
        generations(&state, &session.id).is_empty(),
        "no park within the quiet time"
    );
    c.send(FrameType::Close, 1, &[]).await;
    // The echo matched; cat's own copy of the line may still be on its
    // way ahead of `CLOSED`.
    loop {
        let (header, payload) = c.recv_on(1).await;
        if header.frame_type == FrameType::Closed {
            let closed: Closed = serde_json::from_slice(&payload).unwrap();
            assert_eq!(closed.reason, ClosedReason::Closed);
            break;
        }
        assert_eq!(header.frame_type, FrameType::Output);
    }
    wait_parked(&state, &session.id, "screen.txt", |t| {
        t.contains("detached-line")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graceful_stop_parks_and_a_restart_restores() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    let session = c
        .create(json!({
            "label": "work",
            "argv": ["/bin/sh", "-c", "printf '\\033]2;parked title\\007'; echo hello-park; exec cat"],
        }))
        .await;
    wait_text(&mut c, &session.id, "hello-park").await;
    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 30, "rows": 6}}),
    )
    .await;
    c.bootstrap(1).await;

    a.registry.stop().await;
    // The binary exits right after the stop: the stop has flushed every
    // connection by then.
    a.shutdown();
    assert_eq!(c.expect_closed(1).await, ClosedReason::Stopping);
    assert!(c.recv().await.is_none());
    let generation = generations(&state, &session.id)
        .pop()
        .expect("parked by the stop");
    // Private to the user: directories 0700, files 0600, as the socket.
    let mode = |path: &Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
            & 0o777
    };
    assert_eq!(mode(&generation), 0o700);
    for file in ["session.json", "snapshot.ghostsnp", "screen.txt"] {
        assert!(generation.join(file).is_file(), "{file}");
        assert_eq!(mode(&generation.join(file)), 0o600, "{file}");
    }
    assert_eq!(mode(&a.socket), 0o600);
    drop(c);
    drop(a);

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    let page = c.list().await;
    assert_eq!(page.sessions.len(), 1);
    let restored = &page.sessions[0];
    assert_eq!(restored.id, session.id);
    assert_eq!(restored.status, Status::Restored);
    assert_eq!(restored.label, "work");
    assert_eq!(restored.title, "parked title");
    assert_eq!((restored.cols, restored.rows), (30, 6));
    assert_eq!(restored.attachments, []);
    assert!(restored.exit.is_none());

    let read = c.read(&session.id, 100).await;
    assert!(read.text.contains("hello-park"), "{:?}", read.text);
    assert_eq!(read.status, Status::Restored);
    assert!(read.cursor.is_none(), "no cursor from a park file");
    assert_eq!(
        c.err("send", json!({"session": session.id, "data": "aGk="}))
            .await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err(
            "resize",
            json!({"session": session.id, "cols": 1, "rows": 1})
        )
        .await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err("signal", json!({"session": session.id, "signal": "HUP"}))
            .await,
        ErrorCode::NotRunning
    );

    // OPEN streams the park file as a bootstrap with nothing after it.
    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 80, "rows": 24}}),
    )
    .await;
    let boot = c.bootstrap(1).await;
    assert_eq!(boot.attached.session.status, Status::Restored);
    assert_eq!(boot.attached.mode, AttachMode::Snapshot);
    assert_eq!(
        (boot.attached.cols, boot.attached.rows),
        (30, 6),
        "the parked size"
    );
    assert!(boot.text().contains("hello-park"), "{:?}", boot.text());
    assert_eq!(boot.stream, []);
    c.send(FrameType::Close, 1, &[]).await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
    c.wait_attachments(&session.id, 0).await;

    // Removal deletes the files.
    c.ok("remove", json!({"session": session.id})).await;
    for _ in 0..1000 {
        if !sessions_dir(&state).join(&session.id).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!sessions_dir(&state).join(&session.id).exists());
    assert_eq!(
        c.err("inspect", json!({"session": session.id})).await,
        ErrorCode::NoSuchSession
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_parks_at_exit_and_restores_as_exited() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({
            "on_exit": "keep",
            "argv": ["/bin/sh", "-c",
                "printf '\\033]7501;state=working:app=cargo:id=build\\033\\\\'; printf '\\033]7501;state=error:id=last:app=cargo\\033\\\\'; echo done-keep; exit 3"],
        }))
        .await;
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(exited.exit, Some(Exit::Code { code: 3 }));
    wait_parked(&state, &session.id, "screen.txt", |_| true).await;
    drop(c);
    a.registry.stop().await;
    drop(a);

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    let loaded = c.inspect(&session.id).await;
    assert_eq!(loaded.status, Status::Exited);
    assert_eq!(loaded.exit, Some(Exit::Code { code: 3 }));
    assert_eq!(loaded.on_exit, OnExit::Keep);
    // The working record ended with the stream; the error one persists.
    let records = loaded.program_status.unwrap();
    assert_eq!(
        records
            .iter()
            .map(|r| (r.id.as_str(), r.state.as_str()))
            .collect::<Vec<_>>(),
        vec![("last", "error")]
    );
    assert!(c.read(&session.id, 0).await.text.contains("done-keep"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_parks_an_exited_session_whose_final_park_is_waiting() {
    // A kept session's park at exit queues behind a park in flight. A
    // stop meanwhile makes that queued park a no-op, so the stop's own
    // park must cover the exited session.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"on_exit": "keep", "argv": ["/bin/sh", "-c", "read go; echo held; exit 4"]}))
        .await;
    let park_lock = a
        .registry
        .get(&session.id)
        .unwrap()
        .lock()
        .unwrap()
        .park_lock
        .clone();
    let held = park_lock.lock().await;
    // Not before: the exit's park could finish before the lock is held.
    c.ok("send", json!({"session": session.id, "data": b64("go\n")}))
        .await;
    assert_eq!(
        c.wait_exit(&session.id).await.exit,
        Some(Exit::Code { code: 4 })
    );
    assert_eq!(generations(&state, &session.id), Vec::<PathBuf>::new());
    drop(c);
    let registry = a.registry.clone();
    let stop = tokio::spawn(async move { registry.stop().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    stop.await.unwrap();
    drop(a);

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    let loaded = c.inspect(&session.id).await;
    assert_eq!(
        (loaded.status, loaded.exit),
        (Status::Exited, Some(Exit::Code { code: 4 }))
    );
    assert!(c.read(&session.id, 0).await.text.contains("held"));
}

#[tokio::test(flavor = "multi_thread")]
async fn program_status_records_follow_the_protocol() {
    let server = start(Limits {
        records: 2,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({})).await;
    let id = session.id.clone();
    let report = |body: &str| json!({"session": id, "data": b64(format!("printf '\\033]7501;{body}\\033\\\\'\n"))});

    c.ok("send", report("state=working:app=cargo:progress=40"))
        .await;
    let record = event_where(&mut c, |e| match e {
        Event::ProgramStatusChanged {
            id: sid, record, ..
        } if *sid == session.id => Some(record.clone()),
        _ => None,
    })
    .await;
    assert_eq!((record.id.as_str(), record.state.as_str()), ("", "working"));
    assert_eq!(
        (record.app.as_deref(), record.progress),
        (Some("cargo"), Some(40))
    );
    assert!(record.title.is_none() && record.msg.is_none() && record.kind.is_none());

    c.ok(
        "send",
        report("state=blocked:kind=permission:id=deploy/us:msg=QXBwbHk/"),
    )
    .await;
    let record = event_where(&mut c, |e| match e {
        Event::ProgramStatusChanged { record, .. } if record.id == "deploy/us" => {
            Some(record.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(record.kind.as_deref(), Some("permission"));
    assert_eq!(record.msg.as_deref(), Some("Apply?"));
    assert!(record.app.is_none(), "stored as reported");
    let inspected = c.inspect(&session.id).await;
    let records = inspected.program_status.unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .find(|r| r.id == "deploy/us")
            .unwrap()
            .app
            .as_deref(),
        Some("cargo"),
        "app from the nearest ancestor"
    );

    // A third record evicts the least recently updated one.
    c.ok("send", report("state=done:id=third")).await;
    let removed = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved { record_ids, .. } => Some(record_ids.clone()),
        _ => None,
    })
    .await;
    assert_eq!(removed, vec![String::new()]);

    // A prompt start drops the blocked record, not the done one.
    c.ok(
        "send",
        json!({"session": session.id, "data": b64("printf '\\033]133;A\\033\\\\'\n")}),
    )
    .await;
    let removed = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved { record_ids, .. } => Some(record_ids.clone()),
        _ => None,
    })
    .await;
    assert_eq!(removed, vec!["deploy/us".to_string()]);

    // Clear with no id removes everything.
    c.ok("send", report("state=clear")).await;
    let removed = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved { record_ids, .. } => Some(record_ids.clone()),
        _ => None,
    })
    .await;
    assert_eq!(removed, vec!["third".to_string()]);
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.program_status, Some(vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_lock_refuses_a_second_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let second = || {
        server::Registry::new(server::Config {
            limits: fast_limits(),
            shell: vec!["/bin/sh".into()],
            home: dir.path().to_path_buf(),
            version: "test".into(),
            state_dir: state.clone(),
            socket: dir.path().join("sock"),
            park: server::ParkTiming::default(),
        })
    };
    assert_eq!(
        second().err().map(|e| e.kind()),
        Some(std::io::ErrorKind::WouldBlock),
        "the first server holds the lock"
    );
    a.registry.stop().await;
    assert!(second().is_ok(), "a stopped server has released it");
}

#[tokio::test(flavor = "multi_thread")]
async fn loading_keeps_the_newest_complete_generation_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let id = "11111111-2222-3333-4444-555555555555";
    let session_dir = sessions_dir(&state).join(id);
    let mut file = park_file(id);
    file.history_lines = 1;
    file.session = Session {
        label: "old".into(),
        argv: vec!["/bin/sh".into()],
        pid: Some(1),
        title: "t".into(),
        pwd: "/tmp".into(),
        offset: 7,
        marker: 2,
        program_status: Some(vec![
            record("", "working", None),
            record("k", "done", Some("a")),
        ]),
        ..file.session
    };
    // As 0.3.0 wrote it: a count of attachments, no size.
    let mut old = serde_json::to_value(&file).unwrap();
    old["session"]["attachments"] = 3.into();
    old["session"].as_object_mut().unwrap().remove("size");
    let write = |generation: &str, files: &[&str]| {
        let g = session_dir.join(generation);
        std::fs::create_dir_all(&g).unwrap();
        for name in files {
            let bytes = match *name {
                "session.json" => serde_json::to_vec(&old).unwrap(),
                "screen.txt" => format!("line one\nline two {generation}\n").into_bytes(),
                _ => b"GHOSTSNP".to_vec(),
            };
            std::fs::write(g.join(name), bytes).unwrap();
        }
    };
    write("100", &["session.json", "snapshot.ghostsnp", "screen.txt"]);
    write("200", &["session.json", "snapshot.ghostsnp"]);
    write(".300", &["session.json", "snapshot.ghostsnp", "screen.txt"]);
    write("50", &["session.json", "snapshot.ghostsnp", "screen.txt"]);
    // An unrelated session directory with nothing complete goes away.
    std::fs::create_dir_all(sessions_dir(&state).join("empty").join("1")).unwrap();

    let server = start_with(fast_limits(), state.clone());
    let mut c = client(&server).await;
    let page = c.list().await;
    assert_eq!(page.sessions.len(), 1);
    let s = &page.sessions[0];
    assert_eq!(
        (s.status, s.pid, s.attachments.len(), s.offset, s.marker),
        (Status::Restored, None, 0, 7, 2)
    );
    let names: Vec<String> = std::fs::read_dir(&session_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["100"]);
    assert!(!sessions_dir(&state).join("empty").exists());
    // One history line before the screen.
    assert_eq!(c.read(id, 1).await.text, "line one\nline two 100\n");
    assert_eq!(c.read(id, 0).await.text, "line two 100\n");
    assert_eq!(c.read(id, 5).await.text, "line one\nline two 100\n");
    let inspected = c.inspect(id).await;
    assert_eq!(
        inspected
            .program_status
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        vec!["k".to_string()],
        "the working record is dropped on load"
    );

    // A resume marker above the parked one is invalid, as on a live session.
    c.open(
        1,
        json!({"session": id, "viewport": {"cols": 80, "rows": 24},
        "resume": {"incarnation": server.registry.incarnation, "offset": 7, "marker": 3}}),
    )
    .await;
    assert_eq!(c.expect_error(1).await, ErrorCode::InvalidRequest);
    // The fixture's snapshot is not one: the attach fails and is gone.
    c.open(
        1,
        json!({"session": id, "viewport": {"cols": 80, "rows": 24}}),
    )
    .await;
    assert_eq!(c.expect_error(1).await, ErrorCode::Internal);
    c.wait_attachments(id, 0).await;
}

const OTHER: &str = "99999999-2222-3333-4444-555555555555";

/// A running session's park, as written by this engine and protocol.
fn park_file(id: &str) -> server::park::ParkFile {
    server::park::ParkFile {
        protocol: server::park::PROTOCOL,
        engine: ghostty_vt::ENGINE.into(),
        parked_at: "2026-10-18T00:00:00Z".into(),
        history_lines: 0,
        session: Session {
            id: id.into(),
            label: String::new(),
            status: Status::Running,
            argv: vec!["cat".into()],
            cwd: "/".into(),
            pid: None,
            cols: 20,
            rows: 5,
            title: String::new(),
            pwd: String::new(),
            on_exit: OnExit::Keep,
            created_at: "2026-10-18T00:00:00Z".into(),
            exited_at: None,
            exit: None,
            offset: 0,
            marker: 0,
            attachments: Vec::new(),
            command: None,
            program_status: None,
            attention: None,
            size: SizeInfo::default(),
        },
        commands: Vec::new(),
    }
}

/// A record with nothing but its id, state and app.
fn record(id: &str, state: &str, app: Option<&str>) -> StatusRecord {
    StatusRecord {
        id: id.into(),
        state: state.into(),
        kind: None,
        app: app.map(Into::into),
        title: None,
        msg: None,
        progress: None,
        updated_at: "x".into(),
        seen: None,
    }
}

/// After an engine change only `read` and `remove` work.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_parked_by_another_engine_is_only_read_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let other_dir = sessions_dir(&state).join(OTHER).join("100");
    std::fs::create_dir_all(&other_dir).unwrap();
    let mut other = park_file(OTHER);
    other.engine = "ghostty-vt@0.0.0+none".into();
    std::fs::write(
        other_dir.join("session.json"),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    std::fs::write(other_dir.join("snapshot.ghostsnp"), b"GHOSTSNP").unwrap();
    std::fs::write(other_dir.join("screen.txt"), "elsewhere\n").unwrap();

    let server = start_with(fast_limits(), state.clone());
    let mut c = client(&server).await;
    c.open(
        1,
        json!({"session": OTHER, "viewport": {"cols": 80, "rows": 24}}),
    )
    .await;
    assert_eq!(c.expect_error(1).await, ErrorCode::EngineMismatch);
    assert_eq!(c.read(OTHER, 0).await.text, "elsewhere\n");
    c.ok("remove", json!({"session": OTHER})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_park_naming_another_session_is_skipped() {
    // A `remove` of the restored session would delete `sessions/<id>`
    // for the id the file claims, so a file under the wrong directory is
    // left alone and not restored.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().to_path_buf();
    let dir = sessions_dir(&state).join("mislabeled").join("1");
    std::fs::create_dir_all(&dir).unwrap();
    let file = park_file(OTHER);
    std::fs::write(dir.join("session.json"), serde_json::to_vec(&file).unwrap()).unwrap();
    std::fs::write(dir.join("snapshot.ghostsnp"), b"GHOSTSNP").unwrap();
    std::fs::write(dir.join("screen.txt"), "x\n").unwrap();

    let server = start_with(fast_limits(), state.clone());
    let mut c = client(&server).await;
    let page = c.list().await;
    assert_eq!(page.sessions, []);
    assert!(dir.join("session.json").is_file(), "the directory is kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_server_leaves_the_directory_to_its_successor() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    // `on_exit: remove` with a 200 ms grace: once the stop's SIGHUP ends
    // the process, the old server must not delete the park.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo left-behind; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "left-behind").await;
    a.registry.stop().await;
    // The stop closed the connection, and the old registry accepts
    // nothing new.
    assert!(c.recv().await.is_none(), "closed by the stop");
    assert_eq!(
        a.registry
            .create(CreateParams::default())
            .map(|s| s.id)
            .err(),
        Some((ErrorCode::Internal, "server stopping".into()))
    );

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let page = c.list().await;
    assert_eq!(page.sessions.len(), 1);
    assert!(c.read(&session.id, 0).await.text.contains("left-behind"));
    assert_eq!(generations(&state, &session.id).len(), 1);
    drop(a);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_start_in_the_same_read_still_drops_the_working_record() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf '\\033]7501;state=working:id=w\\033\\\\\\033]133;A\\033\\\\'; echo same-chunk; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "same-chunk").await;
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.program_status, Some(vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_read_takes_scrollback_from_the_history_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone());
    let mut c = client(&a).await;
    let session = c
        .create(json!({"rows": 10, "argv": ["/bin/sh", "-c",
            "seq 1 40; printf '\\033[H\\033[2J'; echo TOP; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "TOP").await;
    let live_screen = c.read(&session.id, 0).await.text;
    let live_two = c.read(&session.id, 2).await.text;
    // Ten rows: the last scroll before the clear left 1 to 31 in history.
    assert_eq!(live_screen, "TOP\n");
    assert_eq!(live_two, "30\n31\nTOP\n");
    a.registry.stop().await;
    drop(c);
    drop(a);

    let b = start_with(fast_limits(), state.clone());
    let mut c = client(&b).await;
    assert_eq!(c.read(&session.id, 0).await.text, live_screen);
    assert_eq!(c.read(&session.id, 2).await.text, live_two);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_remove_keeps_a_parked_session_until_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let limits = Limits {
        remove_grace: 60_000,
        ..fast_limits()
    };
    let a = start_with(limits, state.clone());
    let mut c = client(&a).await;
    // Enough history that a bootstrap pauses for a client that stops
    // reading, which keeps its attachment unfinished.
    let session = c
        .create(json!({
            "on_exit": "keep",
            "argv": ["/bin/sh", "-c",
                "awk 'BEGIN { s = sprintf(\"%1000s\", \"\"); gsub(/ /, \"a\", s); for (i = 0; i < 3000; i++) print s; print \"filled\" }'; exec cat"],
            "cols": 200, "rows": 5,
        }))
        .await;
    wait_text(&mut c, &session.id, "filled").await;
    drop(c);
    a.registry.stop().await;
    drop(a);
    assert!(
        !generations(&state, &session.id).is_empty(),
        "parked by the stop"
    );

    let b = start_with(limits, state.clone());
    let mut c = client(&b).await;
    let mut idle = client(&b).await;
    idle.open(1, json!({"session": session.id})).await;
    idle.attached(1).await;
    c.wait_attachments(&session.id, 1).await;
    c.ok("remove", json!({"session": session.id})).await;
    // Within the grace the session is listed and its files serve a
    // bootstrap.
    c.open(2, json!({"session": session.id})).await;
    assert!(c.bootstrap(2).await.text().contains("filled"));
    // A stop within the grace does not bring it back.
    b.registry.stop().await;
    drop(b);
    let d = start_with(limits, state);
    let mut c = client(&d).await;
    assert_eq!(
        c.err("inspect", json!({"session": session.id})).await,
        ErrorCode::NoSuchSession
    );
}
