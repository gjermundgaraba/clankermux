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
        #[arg(long, value_name = "ORIGIN")]
        origin: Vec<String>,
        /// Certificate chain PEM; without it the node's Tailscale
        /// certificate is used.
        #[arg(long, value_name = "PATH", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// Private key PEM.
        #[arg(long, value_name = "PATH", requires = "tls_cert")]
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

/// Binds the carriers `serve` was asked for and runs them with the Unix
/// socket until a signal; then parks every running session.
async fn serve(socket: PathBuf, carriers: Carriers) -> Result<ExitCode, Error> {
    use std::net::{IpAddr, SocketAddr};
    let Carriers {
        tcp,
        ws,
        origin,
        tls_cert,
        tls_key,
    } = carriers;
    // The tailnet address and certificate domain, when a carrier needs them.
    let api = server::tailscale::LocalApi::discover();
    let tailnet = async |api: &std::io::Result<server::tailscale::LocalApi>| {
        let api = api.as_ref().map_err(|e| usage(e.to_string()))?;
        api.status().await.map_err(Error::Io)
    };
    let tailnet_addr = async |status: &server::tailscale::Status, port: u16| {
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
        let status = tailnet(&api).await?;
        let addr = tailnet_addr(&status, port).await?;
        tcp_listener = Some(
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(Error::Io)?,
        );
    }
    let mut ws_listener = None;
    if let Some(bind) = ws {
        let addr: SocketAddr = match bind.parse::<u16>() {
            Ok(port) => tailnet_addr(&tailnet(&api).await?, port).await?,
            Err(_) => match bind.parse::<SocketAddr>() {
                Ok(addr) if addr.ip().is_loopback() => addr,
                _ => {
                    return Err(usage(format!(
                        "--ws takes a port on the tailnet address or a loopback address, not {bind:?}"
                    )));
                }
            },
        };
        let source = match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => server::tls::Source::Files { cert, key },
            _ => {
                let status = tailnet(&api).await?;
                let domain = status.cert_domains.first().cloned().ok_or_else(|| {
                    usage(
                        "this node has no certificate domain; pass --tls-cert and --tls-key".into(),
                    )
                })?;
                server::tls::Source::Tailscale {
                    api: api.as_ref().unwrap().clone(),
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
    let _ = IpAddr::V4; // keep the import list honest on every platform

    let registry = server::Registry::new(server::default_config()).map_err(Error::Io)?;
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(server::serve_unix(socket.clone(), registry.clone()));
    eprintln!("listening on {}", socket.display());
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
            origin,
        ));
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(Error::Io)?;
    let result = tokio::select! {
        result = tasks.join_next() => result.unwrap().unwrap_or_else(|e| Err(std::io::Error::other(e))),
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };
    // A graceful stop parks every running session first.
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

/// `--stdio`: bytes both ways between stdio and the socket until either
/// side ends.
async fn relay(socket: PathBuf) -> Result<ExitCode, Error> {
    let mut unix = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| usage(format!("cannot connect to {}: {e}", socket.display())))?;
    let mut stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let _ = tokio::io::copy_bidirectional(&mut unix, &mut stdio).await;
    Ok(ExitCode::SUCCESS)
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
