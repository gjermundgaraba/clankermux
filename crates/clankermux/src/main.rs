//! The clankermux binary: the server and the command-line client.

mod attach;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use base64::Engine as _;
use clap::{Args, Parser, Subcommand};
use client::connection::hello;
use client::{Connection, Endpoint, Error};
use proto::{
    CreateParams, LabelParams, ListParams, OnExit, Page, Read, ReadParams, ResizeParams,
    SendParams, Session, SessionParams, SignalParams,
};
use serde::Serialize;

const DETACH_KEY: &str = "C-\\";

/// The session a command inside a session defaults to.
const SESSION_ENV: &str = "CLANKERMUX_SESSION";
/// The socket of the server that holds it.
const SOCKET_ENV: &str = "CLANKERMUX_SOCKET";

/// Exit codes besides 0, 1 and clap's 2.
const EXIT_USAGE: i32 = 2;
const EXIT_TARGET: i32 = 3;
const EXIT_UNREACHABLE: i32 = 4;

#[derive(Parser)]
#[command(version, about = "clankermux terminal multiplexer")]
struct Cli {
    /// Unix socket path; `$CLANKERMUX_SOCKET` by default, except for
    /// `serve`.
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
    Serve(Carriers),
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
    Inspect {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
    },
    /// Attach the terminal to a session.
    Attach {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
        #[arg(long)]
        read_only: bool,
        /// Prefix key (`C-x`); followed by `d` it detaches, twice it is
        /// sent.
        #[arg(long, default_value = DETACH_KEY)]
        detach_key: String,
    },
    /// Hang up the session's process group.
    End {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
    },
    /// Send a signal by name (`TERM`, `INT`, `KILL`, ...).
    Signal {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the value.
        #[arg(value_name = "[SESSION] SIGNAL", num_args = 1..=2, required = true)]
        args: Vec<String>,
    },
    /// Delete an exited session.
    Remove {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
    },
    /// Set the label.
    Label {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the value.
        #[arg(value_name = "[SESSION] LABEL", num_args = 1..=2, required = true)]
        args: Vec<String>,
    },
    /// Set the size: `COLSxROWS`, or `COLSx` or `xROWS` to keep the other.
    Resize {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the value.
        #[arg(value_name = "[SESSION] SIZE", num_args = 1..=2, required = true)]
        args: Vec<String>,
    },
    /// Print the screen, with history lines before it.
    Read {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
        #[arg(long, default_value_t = 0)]
        scrollback: u32,
    },
    /// Send text as typed, or stdin's bytes with `--bytes`.
    Send {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the text.
        #[arg(value_name = "[SESSION] TEXT", num_args = 0..=2)]
        args: Vec<String>,
        /// Press Enter after the text.
        #[arg(long, conflicts_with = "bytes")]
        enter: bool,
        /// Send stdin's bytes as they are.
        #[arg(long)]
        bytes: bool,
    },
    /// Print session events as JSON lines until interrupted.
    Events,
}

/// Why the CLI failed, by exit code: a client error (1, or 2 for a
/// usage error), a session argument that names no session or several (3),
/// or a server that cannot be reached (4).
enum Fail {
    Client(Error),
    Target(String),
    Unreachable(String),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Client(e)
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Client(Error::Io(e))
    }
}

fn parse_detach_key(spec: &str) -> Result<u8, Error> {
    attach::parse_key(spec).ok_or_else(|| {
        Error::Usage(format!(
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
                .ok_or_else(|| {
                    Error::Usage(format!("environment entry {entry:?} is not NAME=value"))
                })
        })
        .collect()
}

/// The CLI's connections all name the engine, so any of them can attach.
async fn connect(endpoint: &Endpoint) -> Result<Connection, Error> {
    endpoint
        .connect(hello("clankermux", Some(ghostty_vt::ENGINE)))
        .await
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

/// The sessions `needle` names, by the first rule that matches any: the
/// full id; the label, ignoring case; a prefix or suffix of the id of at
/// least four characters.
fn matching<'a>(sessions: &'a [Session], needle: &str) -> Vec<&'a Session> {
    let rules: [&dyn Fn(&Session) -> bool; 3] = [
        &|s| s.id == needle,
        &|s| s.label.to_lowercase() == needle.to_lowercase(),
        &|s| needle.chars().count() >= 4 && (s.id.starts_with(needle) || s.id.ends_with(needle)),
    ];
    rules
        .iter()
        .map(|rule| sessions.iter().filter(|s| rule(s)).collect::<Vec<_>>())
        .find(|found| !found.is_empty())
        .unwrap_or_default()
}

/// A session argument as an id; omitted, `$CLANKERMUX_SESSION`. None or
/// several matches fail, listing the candidates.
async fn resolve(conn: &Connection, needle: Option<String>) -> Result<String, Fail> {
    let needle = match needle.or_else(|| std::env::var(SESSION_ENV).ok().filter(|v| !v.is_empty()))
    {
        Some(needle) => needle,
        None => {
            return Err(Fail::Client(Error::Usage(format!(
                "a session is needed: name one, or run inside a session (${SESSION_ENV})"
            ))));
        }
    };
    let exact = SessionParams {
        session: needle.clone(),
    };
    if conn.call::<_, Session>("inspect", &exact).await.is_ok() {
        return Ok(needle);
    }
    let sessions = all_sessions(conn).await?;
    let found = matching(&sessions, &needle);
    let (what, candidates) = match found.as_slice() {
        [s] => return Ok(s.id.clone()),
        [] => ("no session matches", sessions.iter().collect()),
        _ => ("several sessions match", found),
    };
    let mut message = format!("{what} {needle:?}");
    for s in candidates {
        message.push_str(&format!(
            "\n  {}  {}  {}",
            short_id(&s.id),
            s.label,
            s.status
        ));
    }
    Err(Fail::Target(message))
}

/// The last six characters of an id, as listings print it.
fn short_id(id: &str) -> &str {
    id.get(id.len().saturating_sub(6)..).unwrap_or(id)
}

/// Splits `[SESSION] REST...` where REST is `rest` arguments long.
fn target_and(mut args: Vec<String>, rest: usize) -> (Option<String>, Vec<String>) {
    if args.len() > rest {
        let session = args.remove(0);
        (Some(session), args)
    } else {
        (None, args)
    }
}

/// `COLSxROWS`, `COLSx` or `xROWS`.
fn parse_size(spec: &str) -> Result<(Option<u16>, Option<u16>), Error> {
    let bad = || Error::Usage(format!("{spec:?} is not COLSxROWS, COLSx or xROWS"));
    let (cols, rows) = spec.split_once('x').ok_or_else(bad)?;
    let side = |s: &str| {
        (!s.is_empty())
            .then(|| s.parse::<u16>().map_err(|_| bad()))
            .transpose()
    };
    match (side(cols)?, side(rows)?) {
        (None, None) => Err(bad()),
        size => Ok(size),
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
        "{:<6} {:<12} {:<8}{:<9} {:>3}x{:<3} {} {}\n",
        short_id(&s.id),
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
    let Carriers {
        tcp,
        ws,
        origin,
        tls_cert,
        tls_key,
        park_timing,
    } = carriers;
    let origins = origin
        .iter()
        .map(|o| server::ws::origin(o).map_err(Error::Usage))
        .collect::<Result<Vec<_>, _>>()?;
    let ws_bind = ws
        .as_deref()
        .map(|bind| match bind.parse::<u16>() {
            Ok(port) => Ok(WsBind::Port(port)),
            Err(_) => match bind.parse::<SocketAddr>() {
                Ok(addr) if addr.ip().is_loopback() => Ok(WsBind::Loopback(addr)),
                _ => Err(Error::Usage(format!(
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
    let mut config = server::default_config();
    if let Some(park) = park_timing {
        config.park = park;
    }
    let registry = server::Registry::new(config)?;
    // One status request serves every carrier that needs the tailnet.
    let needs_tailnet = tcp.is_some()
        || matches!(ws_bind, Some(WsBind::Port(_)))
        || (ws_bind.is_some() && tls_files.is_none());
    let tailnet = if needs_tailnet {
        let api =
            server::tailscale::LocalApi::discover().map_err(|e| Error::Usage(e.to_string()))?;
        let status = api.status().await?;
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
            .ok_or_else(|| Error::Usage("this node has no tailnet address".into()))?;
        Ok::<SocketAddr, Error>(SocketAddr::new(*ip, port))
    };
    let mut tcp_listener = None;
    if let Some(port) = tcp {
        tcp_listener = Some(tokio::net::TcpListener::bind(tailnet_addr(port)?).await?);
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
                    Error::Usage(
                        "this node has no certificate domain; pass --tls-cert and --tls-key".into(),
                    )
                })?;
                server::tls::Source::Tailscale {
                    api: api.clone(),
                    domain,
                }
            }
        };
        let tls = server::tls::Tls::start(source).await?;
        tokio::spawn(tls.clone().renew_forever());
        let listener = tokio::net::TcpListener::bind(addr).await?;
        ws_listener = Some((listener, tls));
    }

    let mut tasks = tokio::task::JoinSet::new();
    let unix_listener = server::bind_unix(&socket)?;
    eprintln!("listening on {}", socket.display());
    tasks.spawn(server::serve_unix(unix_listener, registry.clone()));
    if let Some(listener) = tcp_listener {
        eprintln!("listening on tcp://{}", listener.local_addr()?);
        tasks.spawn(server::serve_tcp(listener, registry.clone()));
    }
    if let Some((listener, tls)) = ws_listener {
        eprintln!("listening on wss://{}", listener.local_addr()?);
        tasks.spawn(server::ws::serve_ws(
            listener,
            registry.clone(),
            tls,
            origins,
        ));
    }
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let result = tokio::select! {
        result = tasks.join_next() => result.unwrap().unwrap_or_else(|e| Err(std::io::Error::other(e))),
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    };
    // A graceful stop parks every running session and delivers `CLOSED
    // stopping` before the process ends.
    registry.stop().await;
    result?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Args)]
struct Carriers {
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
    /// When running sessions are parked, in seconds (fractions allowed),
    /// for tests: every session every INTERVAL, and a session that
    /// changed once it has been quiet for QUIET or BUSY after its first
    /// change, whichever comes first, but not within GAP of its last
    /// park. Also on the last detach and on stop. [default: 300,2,5,30]
    #[arg(long, value_name = "INTERVAL,QUIET,GAP,BUSY", value_parser = parse_park_timing)]
    park_timing: Option<server::ParkTiming>,
}

fn parse_park_timing(spec: &str) -> Result<server::ParkTiming, String> {
    // Far beyond any use, and far from an `Instant` overflow.
    const YEAR: f64 = 365.0 * 24.0 * 3600.0;
    let seconds: Vec<std::time::Duration> = spec
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<f64>()
                .ok()
                .filter(|s| *s > 0.0 && *s <= YEAR)
                .map(std::time::Duration::from_secs_f64)
                .ok_or_else(|| format!("{s:?} is not a number of seconds above 0, up to a year"))
        })
        .collect::<Result<_, _>>()?;
    match seconds[..] {
        [interval, quiet, gap, busy] => Ok(server::ParkTiming {
            interval,
            quiet,
            gap,
            busy,
        }),
        _ => Err("four durations are needed: INTERVAL,QUIET,GAP,BUSY".into()),
    }
}

/// What `--ws` names: a port on the tailnet address, or a loopback address.
enum WsBind {
    Port(u16),
    Loopback(SocketAddr),
}

/// `--stdio`: bytes both ways between stdio and the socket. The relay ends
/// when the server side does; stdin's blocking read cannot be cancelled,
/// so main exits the process rather than waiting for it.
async fn relay(socket: PathBuf) -> Result<i32, Fail> {
    use tokio::io::AsyncWriteExt as _;
    let unix = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| Fail::Unreachable(format!("cannot connect to {}: {e}", socket.display())))?;
    let (mut from_server, mut to_server) = unix.into_split();
    // Stdin EOF shuts down the socket's write half; the server then ends
    // the connection, and its last bytes end the relay.
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut to_server).await;
        let _ = to_server.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut from_server, &mut stdout).await?;
    stdout.flush().await?;
    Ok(0)
}

/// An attach's outcome as an exit status.
fn status_of(code: ExitCode) -> i32 {
    if code == ExitCode::SUCCESS { 0 } else { 1 }
}

/// Text and stdin go as `send` requests whose base64 fits the server's
/// control body with room for the rest of the JSON, and whose bytes fit
/// the input queue (at least one byte, so an absurd limit still makes
/// progress).
async fn send_bytes(conn: &Connection, session: &str, data: &[u8]) -> Result<(), Error> {
    let limits = &conn.welcome.limits;
    let chunk = ((limits.control_body as usize).saturating_sub(1024) / 4 * 3)
        .min(limits.input_queue as usize)
        .max(1);
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(chunk).collect()
    };
    for chunk in chunks {
        conn.request(
            "send",
            SendParams {
                session: session.to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(chunk),
            },
        )
        .await?;
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<i32, Fail> {
    let json = cli.json;
    let command = match (cli.stdio, cli.command) {
        (true, None) => None,
        (true, Some(_)) => return Err(Error::Usage("--stdio takes no command".into()).into()),
        (false, None) => {
            return Err(Error::Usage("a command is required; see --help".into()).into());
        }
        (false, Some(command)) => Some(command),
    };
    if let Some(Command::Serve(carriers)) = command {
        if cli.connect.is_some() || cli.ssh.is_some() {
            return Err(Error::Usage(
                "serve runs locally; --connect and --ssh do not apply".into(),
            )
            .into());
        }
        let socket = cli.socket.unwrap_or_else(server::socket_path);
        return Ok(status_of(serve(socket, carriers).await?));
    }
    // Inside a session, the server that holds it.
    let socket = cli
        .socket
        .or_else(|| {
            std::env::var_os(SOCKET_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(server::socket_path);
    let Some(command) = command else {
        return relay(socket).await;
    };
    let endpoint = match (cli.connect, cli.ssh) {
        (Some(addr), _) => Endpoint::Tcp(addr),
        (None, Some(destination)) => Endpoint::Ssh(destination),
        (None, None) => Endpoint::Unix(socket),
    };
    let conn = connect(&endpoint).await.map_err(|e| match e {
        Error::Io(_) | Error::Closed => {
            Fail::Unreachable(format!("cannot connect to {endpoint}: {e}"))
        }
        other => other.into(),
    })?;
    match command {
        Command::Serve(_) => unreachable!(),
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
                let code = attach::run(
                    attach::Options {
                        endpoint,
                        session: created.id,
                        read_only: false,
                        detach_key,
                    },
                    conn,
                )
                .await?;
                return Ok(status_of(code));
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
            let session = resolve(&conn, session).await?;
            let value = conn.request("inspect", SessionParams { session }).await?;
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
            let session = resolve(&conn, session).await?;
            let code = attach::run(
                attach::Options {
                    endpoint,
                    session,
                    read_only,
                    detach_key,
                },
                conn,
            )
            .await?;
            return Ok(status_of(code));
        }
        Command::End { session } => {
            let session = resolve(&conn, session).await?;
            conn.request("end", SessionParams { session }).await?;
        }
        Command::Signal { args } => {
            let (session, mut rest) = target_and(args, 1);
            let session = resolve(&conn, session).await?;
            let signal = rest.remove(0);
            conn.request("signal", SignalParams { session, signal })
                .await?;
        }
        Command::Remove { session } => {
            let session = resolve(&conn, session).await?;
            conn.request("remove", SessionParams { session }).await?;
        }
        Command::Label { args } => {
            let (session, mut rest) = target_and(args, 1);
            let session = resolve(&conn, session).await?;
            let label = rest.remove(0);
            let updated: Session = conn.call("label", LabelParams { session, label }).await?;
            print(json, &updated, |_| {});
        }
        Command::Resize { args } => {
            let (session, rest) = target_and(args, 1);
            let (cols, rows) = parse_size(&rest[0])?;
            let session = resolve(&conn, session).await?;
            conn.request(
                "resize",
                ResizeParams {
                    session,
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
            let session = resolve(&conn, session).await?;
            let read: Read = conn
                .call(
                    "read",
                    ReadParams {
                        session,
                        scrollback: Some(scrollback),
                    },
                )
                .await?;
            print(json, &read, |read| emit(&read.text));
        }
        Command::Send { args, enter, bytes } => {
            let (session, rest) = target_and(args, if bytes { 0 } else { 1 });
            if bytes && !rest.is_empty() {
                return Err(Error::Usage("--bytes takes no TEXT".into()).into());
            }
            let data = match rest.into_iter().next() {
                _ if bytes => {
                    let mut data = Vec::new();
                    std::io::stdin().read_to_end(&mut data)?;
                    data
                }
                Some(text) => {
                    let mut data = text.into_bytes();
                    if enter {
                        data.push(b'\r');
                    }
                    data
                }
                None if enter => b"\r".to_vec(),
                None => {
                    return Err(Error::Usage("send takes TEXT, --enter or --bytes".into()).into());
                }
            };
            let session = resolve(&conn, session).await?;
            send_bytes(&conn, &session, &data).await?;
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
                        None => return Err(conn.closed().await.into()),
                    },
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
        }
    }
    Ok(0)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let code = match run(Cli::parse()).await {
        Ok(code) => code,
        Err(fail) => {
            let (message, code) = match fail {
                Fail::Client(Error::Usage(message)) => (message, EXIT_USAGE),
                Fail::Client(err) => (err.to_string(), 1),
                Fail::Target(message) => (message, EXIT_TARGET),
                Fail::Unreachable(message) => (message, EXIT_UNREACHABLE),
            };
            eprintln!("clankermux: {message}");
            code
        }
    };
    // The stdin reader thread of an attach must not keep the process up.
    std::process::exit(code)
}
