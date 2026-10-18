//! Park files, restart, and program status records.

mod common;

use std::time::Duration;

use common::*;
use proto::*;
use serde_json::json;

/// The next event of the kind `pick` accepts.
async fn event_where<T>(c: &mut Client, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
    for _ in 0..50 {
        let event = c.event().await;
        if let Some(out) = pick(&event) {
            return out;
        }
    }
    panic!("no matching event within 50 events");
}

fn sessions_dir(state: &std::path::Path) -> std::path::PathBuf {
    state.join("sessions")
}

/// The complete generation directories of `id`, oldest first.
fn generations(state: &std::path::Path, id: &str) -> Vec<std::path::PathBuf> {
    let mut found: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(sessions_dir(state).join(id))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let generation = entry.file_name().to_string_lossy().parse().ok()?;
                    entry
                        .path()
                        .join("screen.txt")
                        .is_file()
                        .then(|| (generation, entry.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

/// Waits until a complete generation exists for `id`.
async fn wait_parked(state: &std::path::Path, id: &str) -> std::path::PathBuf {
    for _ in 0..500 {
        if let Some(path) = generations(state, id).pop() {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("session {id} was not parked");
}

/// The `read` text of a session.
async fn read(c: &mut Client, id: &str, scrollback: u32) -> Read {
    serde_json::from_value(
        c.ok("read", json!({"session": id, "scrollback": scrollback}))
            .await,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graceful_stop_parks_and_a_restart_restores() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone()).await;
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
    assert_eq!(c.expect_closed(1).await, ClosedReason::Stopping);
    let generation = generations(&state, &session.id)
        .pop()
        .expect("parked by the stop");
    assert!(generation.join("session.json").is_file());
    assert!(generation.join("snapshot.ghostsnp").is_file());
    drop(c);
    drop(a);

    let b = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&b).await;
    let page: Page = serde_json::from_value(c.ok("list", json!({})).await).unwrap();
    assert_eq!(page.sessions.len(), 1);
    let restored = &page.sessions[0];
    assert_eq!(restored.id, session.id);
    assert_eq!(restored.status, Status::Restored);
    assert_eq!(restored.label, "work");
    assert_eq!(restored.title, "parked title");
    assert_eq!((restored.cols, restored.rows), (30, 6));
    assert_eq!(restored.attachments, 0);
    assert!(restored.exit.is_none());

    let read = read(&mut c, &session.id, 100).await;
    assert!(read.text.contains("hello-park"), "{:?}", read.text);
    assert_eq!(read.status, Status::Restored);
    assert!(read.cursor.is_none(), "no cursor from a park file");
    assert_eq!(
        c.err("write", json!({"session": session.id, "data": "aGk="}))
            .await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err(
            "set_size",
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
    assert!(boot.stream.is_empty());
    c.send(FrameType::Close, 1, &[]).await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
    c.wait_attachments(&session.id, 0).await;

    // Removal deletes the files.
    c.ok("remove", json!({"session": session.id})).await;
    for _ in 0..100 {
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
    let a = start_with(fast_limits(), state.clone()).await;
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
    wait_parked(&state, &session.id).await;
    drop(c);
    a.registry.stop().await;
    drop(a);

    let b = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&b).await;
    let loaded: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
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
    let read: Read =
        serde_json::from_value(c.ok("read", json!({"session": session.id})).await).unwrap();
    assert!(read.text.contains("done-keep"));
}

#[tokio::test(flavor = "multi_thread")]
async fn program_status_records_follow_the_protocol() {
    let server = start(Limits {
        records: 2,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({})).await;
    let id = session.id.clone();
    let report = |body: &str| json!({"session": id, "data": base64(&format!("printf '\\033]7501;{body}\\033\\\\'\n"))});

    c.ok("write", report("state=working:app=cargo:progress=40"))
        .await;
    let record = event_where(&mut c, |e| match e {
        Event::ProgramStatusChanged { id: sid, record } if *sid == session.id => {
            Some(record.clone())
        }
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
        "write",
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
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
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
    c.ok("write", report("state=done:id=third")).await;
    let removed = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved { record_ids, .. } => Some(record_ids.clone()),
        _ => None,
    })
    .await;
    assert_eq!(removed, vec![String::new()]);

    // A prompt start drops the blocked record, not the done one.
    c.ok(
        "write",
        json!({"session": session.id, "data": base64("printf '\\033]133;A\\033\\\\'\n")}),
    )
    .await;
    let removed = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved { record_ids, .. } => Some(record_ids.clone()),
        _ => None,
    })
    .await;
    assert_eq!(removed, vec!["deploy/us".to_string()]);

    // Clear with no id removes everything.
    c.ok("write", report("state=clear")).await;
    let removed = event_where(&mut c, |e| match e {
        Event::ProgramStatusRemoved { record_ids, .. } => Some(record_ids.clone()),
        _ => None,
    })
    .await;
    assert_eq!(removed, vec!["third".to_string()]);
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(inspected.program_status, Some(vec![]));
}

fn base64(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(text)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_lock_refuses_a_second_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone()).await;
    let second = server::Registry::new(server::Config {
        limits: fast_limits(),
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
        state_dir: state.clone(),
    });
    assert!(second.is_err(), "the first server holds the lock");
    a.registry.stop().await;
    assert!(
        server::Registry::new(server::Config {
            limits: fast_limits(),
            shell: vec!["/bin/sh".into()],
            home: dir.path().to_path_buf(),
            version: "test".into(),
            state_dir: state,
        })
        .is_ok(),
        "a stopped server has released it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn loading_keeps_the_newest_complete_generation_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let id = "11111111-2222-3333-4444-555555555555";
    let session_dir = sessions_dir(&state).join(id);
    let file = server::park::ParkFile {
        protocol: server::park::PROTOCOL,
        engine: ghostty_vt::ENGINE.into(),
        parked_at: "2026-10-18T00:00:00Z".into(),
        history_lines: 1,
        session: Session {
            id: id.into(),
            label: "old".into(),
            status: Status::Running,
            argv: vec!["/bin/sh".into()],
            cwd: "/".into(),
            pid: Some(1),
            cols: 20,
            rows: 5,
            title: "t".into(),
            pwd: "/tmp".into(),
            on_exit: OnExit::Keep,
            created_at: "2026-10-18T00:00:00Z".into(),
            exited_at: None,
            exit: None,
            offset: 7,
            marker: 2,
            attachments: 3,
            program_status: Some(vec![
                StatusRecord {
                    id: "".into(),
                    state: "working".into(),
                    kind: None,
                    app: None,
                    title: None,
                    msg: None,
                    progress: None,
                    updated_at: "x".into(),
                },
                StatusRecord {
                    id: "k".into(),
                    state: "done".into(),
                    kind: None,
                    app: Some("a".into()),
                    title: None,
                    msg: None,
                    progress: None,
                    updated_at: "x".into(),
                },
            ]),
        },
    };
    let write = |generation: &str, files: &[&str]| {
        let g = session_dir.join(generation);
        std::fs::create_dir_all(&g).unwrap();
        for name in files {
            let bytes = match *name {
                "session.json" => serde_json::to_vec(&file).unwrap(),
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
    // A session parked by another engine.
    let other_dir = sessions_dir(&state).join(OTHER).join("100");
    std::fs::create_dir_all(&other_dir).unwrap();
    let mut other = file.clone();
    other.engine = "ghostty-vt@0.0.0+none".into();
    other.history_lines = 0;
    other.session.id = OTHER.into();
    std::fs::write(
        other_dir.join("session.json"),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    std::fs::write(other_dir.join("snapshot.ghostsnp"), b"GHOSTSNP").unwrap();
    std::fs::write(other_dir.join("screen.txt"), "elsewhere\n").unwrap();

    let server = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&server).await;
    let page: Page = serde_json::from_value(c.ok("list", json!({})).await).unwrap();
    assert_eq!(page.sessions.len(), 2);
    let s = page.sessions.iter().find(|s| s.id == id).unwrap();
    assert_eq!(
        (s.status, s.pid, s.attachments, s.offset, s.marker),
        (Status::Restored, None, 0, 7, 2)
    );
    let names: Vec<String> = std::fs::read_dir(&session_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["100"]);
    assert!(!sessions_dir(&state).join("empty").exists());
    // One history line before the screen.
    assert_eq!(read(&mut c, id, 1).await.text, "line one\nline two 100\n");
    assert_eq!(read(&mut c, id, 0).await.text, "line two 100\n");
    assert_eq!(read(&mut c, id, 5).await.text, "line one\nline two 100\n");
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": id})).await).unwrap();
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

    // After an engine change only `read` and `remove` work.
    c.open(
        1,
        json!({"session": OTHER, "viewport": {"cols": 80, "rows": 24}}),
    )
    .await;
    assert_eq!(c.expect_error(1).await, ErrorCode::EngineMismatch);
    assert_eq!(read(&mut c, OTHER, 0).await.text, "elsewhere\n");
    c.ok("remove", json!({"session": OTHER})).await;
}

const OTHER: &str = "99999999-2222-3333-4444-555555555555";

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_server_leaves_the_directory_to_its_successor() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&a).await;
    // `on_exit: remove` with a 200 ms grace: once the stop's SIGHUP ends
    // the process, the old server must not delete the park.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo left-behind; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "left-behind").await;
    a.registry.stop().await;
    // And the old registry accepts nothing new.
    assert_eq!(c.err("create", json!({})).await, ErrorCode::Internal);
    drop(c);

    let b = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&b).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let page: Page = serde_json::from_value(c.ok("list", json!({})).await).unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert!(
        read(&mut c, &session.id, 0)
            .await
            .text
            .contains("left-behind")
    );
    assert_eq!(generations(&state, &session.id).len(), 1);
    drop(a);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_start_in_the_same_read_still_drops_the_working_record() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf '\\033]7501;state=working:id=w\\033\\\\\\033]133;A\\033\\\\'; echo same-chunk; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "same-chunk").await;
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(inspected.program_status, Some(vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_read_takes_scrollback_from_the_history_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let a = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&a).await;
    let session = c
        .create(json!({"rows": 10, "argv": ["/bin/sh", "-c",
            "seq 1 40; printf '\\033[H\\033[2J'; echo TOP; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "TOP").await;
    let live_screen = read(&mut c, &session.id, 0).await.text;
    let live_two = read(&mut c, &session.id, 2).await.text;
    // Ten rows: the last scroll before the clear left 1 to 31 in history.
    assert_eq!(live_screen, "TOP\n");
    assert_eq!(live_two, "30\n31\nTOP\n");
    a.registry.stop().await;
    drop(c);
    drop(a);

    let b = start_with(fast_limits(), state.clone()).await;
    let mut c = client(&b).await;
    assert_eq!(read(&mut c, &session.id, 0).await.text, live_screen);
    assert_eq!(read(&mut c, &session.id, 2).await.text, live_two);
}
