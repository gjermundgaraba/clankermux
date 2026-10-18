//! The clankermux binary: the server and the command-line client.

mod attach;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use client::connection::hello;
use client::{Connection, Error};
use proto::{
    CreateParams, ListParams, OnExit, Page, Read, ReadParams, Session, SessionParams,
    SetLabelParams, SetSizeParams, SignalParams, WriteParams,
};
use serde::Serialize;

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
        /// Prefix key for `--attach` (`C-x`); followed by `d` it detaches,
        /// twice it is sent.
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
        /// Prefix key (`C-x`); followed by `d` it detaches, twice it is
        /// sent.
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

fn usage(message: String) -> Error {
    Error::Usage(message)
}

fn parse_detach_key(spec: &str) -> Result<u8, Error> {
    attach::parse_key(spec).ok_or_else(|| {
        usage(format!(
            "unusable detach key {spec:?}: use C-<letter> or C-<@ [ \\ ] ^ _ ?>"
        ))
    })
}

fn parse_env(entries: &[String]) -> Result<BTreeMap<String, String>, Error> {
    entries
        .iter()
        .map(|entry| {
            entry
                .split_once('=')
                .filter(|(name, _)| !name.is_empty())
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .ok_or_else(|| usage(format!("environment entry {entry:?} is not NAME=value")))
        })
        .collect()
}

/// Every session, page by page.
async fn all_sessions(conn: &Connection) -> Result<Vec<Session>, Error> {
    let mut after: Option<String> = None;
    let mut sessions = Vec::new();
    loop {
        let page: Page = conn
            .call(
                "list",
                ListParams {
                    after: after.take(),
                },
            )
            .await?;
        sessions.extend(page.sessions);
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(sessions),
        }
    }
}

/// A session argument: an id, a unique id prefix, or a unique label.
async fn resolve(conn: &Connection, needle: &str) -> Result<SessionParams, Error> {
    let sessions = all_sessions(conn).await?;
    if sessions.iter().any(|s| s.id == needle) {
        return Ok(SessionParams {
            session: needle.to_string(),
        });
    }
    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.id.starts_with(needle) || s.label == needle)
        .collect();
    match matches.as_slice() {
        [s] => Ok(SessionParams {
            session: s.id.clone(),
        }),
        [] => Err(usage(format!("no session matches {needle:?}"))),
        _ => Err(usage(format!("{needle:?} matches several sessions"))),
    }
}

/// Writes to stdout; a closed pipe (`| head`) ends the program quietly.
fn emit(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if let Err(err) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("clankermux: {err}");
        std::process::exit(1);
    }
}

fn print<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T)) {
    if json {
        emit(&format!(
            "{}\n",
            serde_json::to_string_pretty(value).unwrap()
        ));
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
    emit(&format!(
        "{:<8} {:<12} {:<8}{:<9} {:>3}x{:<3} {} {}\n",
        &s.id[..8],
        s.label,
        format!("{:?}", s.status).to_lowercase(),
        exit,
        s.cols,
        s.rows,
        s.title,
        s.pwd
    ));
}

async fn run(cli: Cli) -> Result<ExitCode, Error> {
    let socket = cli.socket.unwrap_or_else(server::socket_path);
    let json = cli.json;
    if let Command::Serve = cli.command {
        let registry = server::Registry::new(server::default_config()).map_err(Error::Io)?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(Error::Io)?;
        let result = tokio::select! {
            result = server::serve_unix(socket, registry.clone()) => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = terminate.recv() => Ok(()),
        };
        // A graceful stop parks every running session first.
        registry.stop().await;
        return result.map(|()| ExitCode::SUCCESS).map_err(Error::Io);
    }
    let conn = Connection::connect_unix(&socket, hello("clankermux", None))
        .await
        .map_err(|e| match e {
            Error::Io(e) => usage(format!("cannot connect to {}: {e}", socket.display())),
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
            let params = CreateParams {
                label,
                argv: (!argv.is_empty()).then_some(argv),
                cwd,
                env: Some(parse_env(&env)?),
                cols,
                rows,
                on_exit: keep.then_some(OnExit::Keep),
            };
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
            print(json, &created, |s| emit(&format!("{}\n", s.id)));
        }
        Command::List => {
            let sessions = all_sessions(&conn).await?;
            print(json, &sessions, |sessions| {
                sessions.iter().for_each(session_line)
            });
        }
        Command::Inspect { session } => {
            let params = resolve(&conn, &session).await?;
            let value = conn.request("inspect", params).await?;
            emit(&format!(
                "{}\n",
                serde_json::to_string_pretty(&value).unwrap()
            ));
        }
        Command::Attach {
            session,
            read_only,
            detach_key,
        } => {
            let detach_key = parse_detach_key(&detach_key)?;
            let params = resolve(&conn, &session).await?;
            drop(conn);
            return attach::run(attach::Options {
                socket,
                session: params.session,
                read_only,
                detach_key,
            })
            .await;
        }
        Command::End { session } => {
            let params = resolve(&conn, &session).await?;
            conn.request("end", params).await?;
        }
        Command::Signal { session, signal } => {
            let params = resolve(&conn, &session).await?;
            conn.request(
                "signal",
                SignalParams {
                    session: params.session,
                    signal,
                },
            )
            .await?;
        }
        Command::Remove { session } => {
            let params = resolve(&conn, &session).await?;
            conn.request("remove", params).await?;
        }
        Command::Label { session, label } => {
            let params = resolve(&conn, &session).await?;
            let updated: Session = conn
                .call(
                    "set_label",
                    SetLabelParams {
                        session: params.session,
                        label,
                    },
                )
                .await?;
            print(json, &updated, |_| {});
        }
        Command::Resize {
            session,
            cols,
            rows,
        } => {
            let params = resolve(&conn, &session).await?;
            conn.request(
                "set_size",
                SetSizeParams {
                    session: params.session,
                    cols,
                    rows,
                },
            )
            .await?;
        }
        Command::Read {
            session,
            scrollback,
        } => {
            let params = resolve(&conn, &session).await?;
            let read: Read = conn
                .call(
                    "read",
                    ReadParams {
                        session: params.session,
                        scrollback: Some(scrollback),
                    },
                )
                .await?;
            print(json, &read, |read| emit(&read.text));
        }
        Command::Write { session, text } => {
            let params = resolve(&conn, &session).await?;
            let data = match text {
                Some(text) => text.into_bytes(),
                None => {
                    let mut data = Vec::new();
                    std::io::stdin().read_to_end(&mut data)?;
                    data
                }
            };
            conn.request(
                "write",
                WriteParams {
                    session: params.session,
                    data: base64::engine::general_purpose::STANDARD.encode(data),
                },
            )
            .await?;
        }
        Command::Events => {
            let mut events = conn.events();
            let page: Page = conn.call("subscribe", ListParams::default()).await?;
            print(json, &page.sessions, |sessions| {
                sessions.iter().for_each(session_line)
            });
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(event) => emit(&format!("{}\n", serde_json::to_string(&event).unwrap())),
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
