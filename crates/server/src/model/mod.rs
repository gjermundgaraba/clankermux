//! Conformance with the Lean models in `lean/`: random operations through
//! the server's code and through a model must be observed the same. The
//! tests need the model's binary in `CLANKERMUX_MODEL`; `just lean`
//! builds it and runs them.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

mod attention;

struct Model {
    /// Ends when the tests close its stdin.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// What the model observes after each operation of one case, a model
/// and its operations as `Main.lean`'s `Case` reads them.
fn observe(case: &Value) -> Vec<Value> {
    static MODEL: OnceLock<Mutex<Model>> = OnceLock::new();
    let model = MODEL.get_or_init(|| {
        let path = std::env::var("CLANKERMUX_MODEL")
            .expect("CLANKERMUX_MODEL names the Lean model's binary; `just lean` sets it");
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the Lean model starts");
        Mutex::new(Model {
            stdin: child.stdin.take().expect("piped"),
            stdout: BufReader::new(child.stdout.take().expect("piped")),
            _child: child,
        })
    });
    let mut line = String::new();
    {
        let mut model = model.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(model.stdin, "{case}").expect("the model reads");
        model.stdin.flush().expect("the model reads");
        model
            .stdout
            .read_line(&mut line)
            .expect("the model answers");
    }
    assert!(!line.starts_with("error:"), "{case}: {line}");
    serde_json::from_str(&line).expect("the model answers JSON")
}
