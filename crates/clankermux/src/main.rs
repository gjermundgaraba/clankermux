//! The clankermux binary: the server and the command-line client.

mod attach;

use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use client::connection::hello;
use client::{Connection, Error};
use proto::{Page, Session};
use serde_json::{Value, json};

const DETACH_KEY: &str = "C-\\";

#[derive(Parser)]
#[command(version, about = "clankermux terminal multiplexer")]
struct Cli {
    /// Unix socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print replies as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve,
    /// Start a session; `argv` after `--`, the login shell by default.
    Create {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        /// Keep the session listed after it exits.
        #[arg(long)]
        keep: bool,
        /// Environment entries, `NAME=value`.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Attach once created.
        #[arg(long)]
        attach: bool,
        /// Prefix key for `--attach`; followed by `d` it detaches, twice
        /// it is sent.
        #[arg(long, default_value = DETACH_KEY)]
        detach_key: String,
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// List sessions.
    List,
    /// Show one session with its program status.
    Inspect { session: String },
    /// Attach the terminal to a session.
    Attach {
        session: String,
        #[arg(long)]
        read_only: bool,
        /// Prefix key; followed by `d` it detaches, twice it is sent.
        #[arg(long, default_value = DETACH_KEY)]
        detach_key: String,
    },
    /// Hang up the session's process group.
    End { session: String },
    /// Send a signal by name.
    Signal { session: String, signal: String },
    /// Delete an exited session.
    Remove { session: String },
    /// Set the label.
    Label { session: String, label: String },
    /// Set the size.
    Resize {
        session: String,
        cols: u16,
        rows: u16,
    },
    /// Print the screen, with history lines before it.
    Read {
        session: String,
        #[arg(long, default_value_t = 0)]
        scrollback: u32,
    },
    /// Write text (or stdin) to the session.
    Write {
        session: String,
        text: Option<String>,
    },
    /// Print session events as JSON lines until interrupted.
    Events,
}

fn parse_detach_key(spec: &str) -> Result<u8, Error> {
    attach::parse_key(spec).ok_or_else(|| Error::Protocol(format!("unusable detach key {spec:?}")))
}

/// A session argument: an id, a unique id prefix, or a unique label.
async fn resolve(conn: &Connection, needle: &str) -> Result<String, Error> {
    let mut after: Option<String> = None;
    let mut matches = Vec::new();
    loop {
        let page: Page = conn.call("list", json!({"after": after})).await?;
        for s in page.sessions {
            if s.id == needle {
                return Ok(s.id);
            }
            if s.id.starts_with(needle) || s.label == needle {
                matches.push(s.id);
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    match matches.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(Error::Protocol(format!("no session matches {needle:?}"))),
        _ => Err(Error::Protocol(format!(
            "{needle:?} matches several sessions"
        ))),
    }
}

fn print(json: bool, value: &Value, human: impl FnOnce(&Value)) {
    if json {
        println!("{}", serde_json::to_string_pretty(value).unwrap());
    } else {
        human(value);
    }
}

fn session_line(s: &Session) {
    let exit = match &s.exit {
        Some(proto::Exit::Code { code }) => format!(" exit {code}"),
        Some(proto::Exit::Signal { signal }) => format!(" {signal}"),
        None => String::new(),
    };
    println!(
        "{:<8} {:<12} {:<8}{:<9} {:>3}x{:<3} {} {}",
        &s.id[..8],
        s.label,
        format!("{:?}", s.status).to_lowercase(),
        exit,
        s.cols,
        s.rows,
        s.title,
        s.pwd
    );
}

async fn run(cli: Cli) -> Result<ExitCode, Error> {
    let socket = cli.socket.unwrap_or_else(server::socket_path);
    let json = cli.json;
    if let Command::Serve = cli.command {
        let registry = server::Registry::new(server::default_config());
        let result = tokio::select! {
            result = server::serve_unix(socket, registry) => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
        };
        return result.map(|()| ExitCode::SUCCESS).map_err(Error::Io);
    }
    let conn = Connection::connect_unix(&socket, hello("clankermux", None))
        .await
        .map_err(|e| match e {
            Error::Io(e) => Error::Protocol(format!("cannot connect to {}: {e}", socket.display())),
            other => other,
        })?;
    match cli.command {
        Command::Serve => unreachable!(),
        Command::Create {
            label,
            cwd,
            cols,
            rows,
            keep,
            env,
            attach,
            detach_key,
            argv,
        } => {
            let detach_key = parse_detach_key(&detach_key)?;
            let env: serde_json::Map<String, Value> = env
                .iter()
                .map(|entry| {
                    let (name, value) = entry.split_once('=').unwrap_or((entry, ""));
                    (name.to_string(), json!(value))
                })
                .collect();
            let mut params = json!({"env": env});
            let fields = [
                ("label", json!(label)),
                ("cwd", json!(cwd)),
                ("cols", json!(cols)),
                ("rows", json!(rows)),
                ("on_exit", json!(keep.then_some("keep"))),
                ("argv", json!((!argv.is_empty()).then_some(argv))),
            ];
            for (name, value) in fields {
                if !value.is_null() {
                    params[name] = value;
                }
            }
            let created: Session = conn.call("create", params).await?;
            if attach {
                drop(conn);
                return attach::run(attach::Options {
                    socket,
                    session: created.id,
                    read_only: false,
                    detach_key,
                })
                .await;
            }
            print(json, &json!(created), |_| println!("{}", created.id));
        }
        Command::List => {
            let mut after: Option<String> = None;
            loop {
                let page: Page = conn.call("list", json!({"after": after})).await?;
                print(json, &json!(page), |_| {
                    page.sessions.iter().for_each(session_line)
                });
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Command::Inspect { session } => {
            let id = resolve(&conn, &session).await?;
            let value = conn.request("inspect", json!({"session": id})).await?;
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
        }
        Command::Attach {
            session,
            read_only,
            detach_key,
        } => {
            let id = resolve(&conn, &session).await?;
            let detach_key = parse_detach_key(&detach_key)?;
            drop(conn);
            return attach::run(attach::Options {
                socket,
                session: id,
                read_only,
                detach_key,
            })
            .await;
        }
        Command::End { session } => {
            let id = resolve(&conn, &session).await?;
            conn.request("end", json!({"session": id})).await?;
        }
        Command::Signal { session, signal } => {
            let id = resolve(&conn, &session).await?;
            conn.request("signal", json!({"session": id, "signal": signal}))
                .await?;
        }
        Command::Remove { session } => {
            let id = resolve(&conn, &session).await?;
            conn.request("remove", json!({"session": id})).await?;
        }
        Command::Label { session, label } => {
            let id = resolve(&conn, &session).await?;
            let value = conn
                .request("set_label", json!({"session": id, "label": label}))
                .await?;
            print(json, &value, |_| {});
        }
        Command::Resize {
            session,
            cols,
            rows,
        } => {
            let id = resolve(&conn, &session).await?;
            conn.request(
                "set_size",
                json!({"session": id, "cols": cols, "rows": rows}),
            )
            .await?;
        }
        Command::Read {
            session,
            scrollback,
        } => {
            let id = resolve(&conn, &session).await?;
            let value = conn
                .request("read", json!({"session": id, "scrollback": scrollback}))
                .await?;
            print(json, &value, |v| {
                print!("{}", v["text"].as_str().unwrap_or(""));
            });
        }
        Command::Write { session, text } => {
            let id = resolve(&conn, &session).await?;
            let data = match text {
                Some(text) => text.into_bytes(),
                None => {
                    let mut data = Vec::new();
                    std::io::stdin().read_to_end(&mut data)?;
                    data
                }
            };
            let data = base64::engine::general_purpose::STANDARD.encode(data);
            conn.request("write", json!({"session": id, "data": data}))
                .await?;
        }
        Command::Events => {
            let mut events = conn.events();
            let page: Page = conn.call("subscribe", json!({})).await?;
            print(json, &json!(page), |_| {
                page.sessions.iter().for_each(session_line)
            });
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(event) => println!("{}", serde_json::to_string(&event).unwrap()),
                        None => return Err(conn.closed().await),
                    },
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let code = match run(Cli::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("clankermux: {err}");
            ExitCode::FAILURE
        }
    };
    // The stdin reader thread of an attach must not keep the process up.
    std::process::exit(if code == ExitCode::SUCCESS { 0 } else { 1 })
}
