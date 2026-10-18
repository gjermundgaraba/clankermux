//! The clankermux binary: the server and the command-line client.

mod attach;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use client::connection::hello;
use client::{Connection, Endpoint, Error};
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
    /// Connect over TCP to `HOST:PORT` on the tailnet.
    #[arg(long, global = true, value_name = "HOST:PORT", conflicts_with_all = ["socket", "ssh"])]
    connect: Option<String>,
    /// Connect through `ssh DESTINATION clankermux --stdio`.
    #[arg(
        long,
        global = true,
        value_name = "DESTINATION",
        conflicts_with = "socket"
    )]
    ssh: Option<String>,
    /// Print replies as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Relay between stdio and the Unix socket, for `ssh`.
    #[arg(long, conflicts_with_all = ["connect", "ssh", "json"])]
    stdio: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Also listen on the tailnet address, plaintext.
        #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "7437")]
        tcp: Option<u16>,
        /// Also serve TLS WebSocket: `PORT` on the tailnet address, or
        /// `127.0.0.1:PORT` / `[::1]:PORT`.
        #[arg(long, value_name = "BIND", num_args = 0..=1, default_missing_value = "7438")]
        ws: Option<String>,
        /// A page origin allowed on the WebSocket (repeatable); none
        /// refuses every page.
        #[arg(long, value_name = "ORIGIN", requires = "ws")]
        origin: Vec<String>,
        /// Certificate chain PEM; without it the node's Tailscale
        /// certificate is used.
        #[arg(long, value_name = "PATH", requires = "ws", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// Private key PEM.
        #[arg(long, value_name = "PATH", requires = "ws", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
    },
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
    let first = conn.call("list", ListParams::default()).await?;
    following_pages(conn, first).await
}

/// `first` and the pages after it.
async fn following_pages(conn: &Connection, mut page: Page) -> Result<Vec<Session>, Error> {
    let mut sessions = Vec::new();
    loop {
        sessions.extend(page.sessions);
        let Some(after) = page.next else {
            return Ok(sessions);
        };
        page = conn.call("list", ListParams { after: Some(after) }).await?;
    }
}

/// A session argument: an id, a unique id prefix, or a unique label.
async fn resolve(conn: &Connection, needle: &str) -> Result<SessionParams, Error> {
    let exact = SessionParams {
        session: needle.to_string(),
    };
    if conn.call::<_, Session>("inspect", &exact).await.is_ok() {
        return Ok(exact);
    }
    let sessions = all_sessions(conn).await?;
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
        s.status.to_string(),
        exit,
        s.cols,
        s.rows,
        s.title,
        s.pwd
    ));
}

/// Binds the carriers `serve` was asked for and runs them with the Unix
/// socket until a signal; then parks every running session.
async fn serve(socket: PathBuf, carriers: Carriers) -> Result<ExitCode, Error> {
    use std::net::SocketAddr;
    let Carriers {
        tcp,
        ws,
        origin,
        tls_cert,
        tls_key,
    } = carriers;
    let origins = origin
        .iter()
        .map(|o| server::ws::origin(o).map_err(usage))
        .collect::<Result<Vec<_>, _>>()?;
    let ws_bind = ws
        .as_deref()
        .map(|bind| match bind.parse::<u16>() {
            Ok(port) => Ok(WsBind::Port(port)),
            Err(_) => match bind.parse::<SocketAddr>() {
                Ok(addr) if addr.ip().is_loopback() => Ok(WsBind::Loopback(addr)),
                _ => Err(usage(format!(
                    "--ws takes a port on the tailnet address or a loopback address, not {bind:?}"
                ))),
            },
        })
        .transpose()?;
    let tls_files = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => Some(server::tls::Source::Files { cert, key }),
        _ => None,
    };
    // The state lock comes first: a second server learns of the first
    // before any tailnet work.
    let registry = server::Registry::new(server::default_config()).map_err(Error::Io)?;
    // One status request serves every carrier that needs the tailnet.
    let needs_tailnet = tcp.is_some()
        || matches!(ws_bind, Some(WsBind::Port(_)))
        || (ws_bind.is_some() && tls_files.is_none());
    let tailnet = if needs_tailnet {
        let api = server::tailscale::LocalApi::discover().map_err(|e| usage(e.to_string()))?;
        let status = api.status().await.map_err(Error::Io)?;
        Some((api, status))
    } else {
        None
    };
    let tailnet_addr = |port: u16| {
        let (_, status) = tailnet.as_ref().expect("needed, so requested");
        let ip = status
            .this
            .tailscale_ips
            .iter()
            .find(|ip| ip.is_ipv4())
            .or(status.this.tailscale_ips.first())
            .ok_or_else(|| usage("this node has no tailnet address".into()))?;
        Ok::<SocketAddr, Error>(SocketAddr::new(*ip, port))
    };
    let mut tcp_listener = None;
    if let Some(port) = tcp {
        tcp_listener = Some(
            tokio::net::TcpListener::bind(tailnet_addr(port)?)
                .await
                .map_err(Error::Io)?,
        );
    }
    let mut ws_listener = None;
    if let Some(bind) = ws_bind {
        let addr = match bind {
            WsBind::Port(port) => tailnet_addr(port)?,
            WsBind::Loopback(addr) => addr,
        };
        let source = match tls_files {
            Some(files) => files,
            None => {
                let (api, status) = tailnet.as_ref().expect("needed, so requested");
                let domain = status.cert_domains.first().cloned().ok_or_else(|| {
                    usage(
                        "this node has no certificate domain; pass --tls-cert and --tls-key".into(),
                    )
                })?;
                server::tls::Source::Tailscale {
                    api: api.clone(),
                    domain,
                }
            }
        };
        let tls = server::tls::Tls::start(source).await.map_err(Error::Io)?;
        tokio::spawn(tls.clone().renew_forever());
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(Error::Io)?;
        ws_listener = Some((listener, tls));
    }

    let mut tasks = tokio::task::JoinSet::new();
    let unix_listener = server::bind_unix(&socket).map_err(Error::Io)?;
    eprintln!("listening on {}", socket.display());
    tasks.spawn(server::serve_unix(unix_listener, registry.clone()));
    if let Some(listener) = tcp_listener {
        eprintln!(
            "listening on tcp://{}",
            listener.local_addr().map_err(Error::Io)?
        );
        tasks.spawn(server::serve_tcp(listener, registry.clone()));
    }
    if let Some((listener, tls)) = ws_listener {
        eprintln!(
            "listening on wss://{}",
            listener.local_addr().map_err(Error::Io)?
        );
        tasks.spawn(server::ws::serve_ws(
            listener,
            registry.clone(),
            tls,
            origins,
        ));
    }
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).map_err(Error::Io)?;
    let mut hangup = signal(SignalKind::hangup()).map_err(Error::Io)?;
    let result = tokio::select! {
        result = tasks.join_next() => result.unwrap().unwrap_or_else(|e| Err(std::io::Error::other(e))),
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    };
    // A graceful stop parks every running session and delivers `CLOSED
    // stopping` before the process ends.
    registry.stop().await;
    result.map(|()| ExitCode::SUCCESS).map_err(Error::Io)
}

struct Carriers {
    tcp: Option<u16>,
    ws: Option<String>,
    origin: Vec<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
}

/// What `--ws` names: a port on the tailnet address, or a loopback address.
enum WsBind {
    Port(u16),
    Loopback(std::net::SocketAddr),
}

/// `--stdio`: bytes both ways between stdio and the socket. The relay ends
/// when the server side does; stdin's blocking read cannot be cancelled,
/// so main exits the process rather than waiting for it.
async fn relay(socket: PathBuf) -> Result<ExitCode, Error> {
    use tokio::io::AsyncWriteExt as _;
    let unix = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| usage(format!("cannot connect to {}: {e}", socket.display())))?;
    let (mut from_server, mut to_server) = unix.into_split();
    // Stdin EOF shuts down the socket's write half; the server then ends
    // the connection, and its last bytes end the relay.
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut to_server).await;
        let _ = to_server.shutdown().await;
    });
    let copied = async {
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut from_server, &mut stdout).await?;
        stdout.flush().await
    }
    .await;
    copied.map(|_| ExitCode::SUCCESS).map_err(Error::Io)
}

async fn run(cli: Cli) -> Result<ExitCode, Error> {
    let socket = cli.socket.unwrap_or_else(server::socket_path);
    let json = cli.json;
    let command = match (cli.stdio, cli.command) {
        (true, None) => return relay(socket).await,
        (true, Some(_)) => return Err(usage("--stdio takes no command".into())),
        (false, None) => return Err(usage("a command is required; see --help".into())),
        (false, Some(command)) => command,
    };
    if let Command::Serve {
        tcp,
        ws,
        origin,
        tls_cert,
        tls_key,
    } = command
    {
        if cli.connect.is_some() || cli.ssh.is_some() {
            return Err(usage(
                "serve runs locally; --connect and --ssh do not apply".into(),
            ));
        }
        return serve(
            socket,
            Carriers {
                tcp,
                ws,
                origin,
                tls_cert,
                tls_key,
            },
        )
        .await;
    }
    let endpoint = match (cli.connect, cli.ssh) {
        (Some(addr), _) => Endpoint::Tcp(addr),
        (None, Some(destination)) => Endpoint::Ssh(destination),
        (None, None) => Endpoint::Unix(socket),
    };
    let conn = endpoint
        .connect(hello("clankermux", None))
        .await
        .map_err(|e| match e {
            Error::Io(e) => usage(format!("cannot connect to {endpoint}: {e}")),
            other => other,
        })?;
    match command {
        Command::Serve { .. } => unreachable!(),
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
                    endpoint,
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
                endpoint,
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
            // Each request's base64 fits the server's control body with
            // room for the rest of the JSON.
            let chunk = (conn.welcome.limits.control_body as usize).saturating_sub(1024) / 4 * 3;
            for chunk in data.chunks(chunk.max(3)) {
                conn.request(
                    "write",
                    WriteParams {
                        session: params.session.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(chunk),
                    },
                )
                .await?;
            }
        }
        Command::Events => {
            let mut events = conn.events();
            let page: Page = conn.call("subscribe", ListParams::default()).await?;
            let sessions = following_pages(&conn, page).await?;
            print(json, &sessions, |sessions| {
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
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
