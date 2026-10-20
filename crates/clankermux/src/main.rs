//! The clankermux binary: the server and the command-line client.

mod attach;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;

use base64::Engine as _;
use clap::{Args, Parser, Subcommand};
use client::connection::hello;
use client::{Connection, Endpoint, Error};
use proto::catalog::{Catalog, DescribeParams, Description};
use proto::{
    AckParams, Acked, CancelParams, CreateParams, CursorParams, DetachParams, LabelParams,
    ListParams, OnExit, OutputMatch, Page, Read, ReadFormat, ReadParams, ReadScope, ResizeParams,
    SendParams, SendWait, Sent, Session, SessionParams, SignalParams, SizePolicy, StatusMatch,
    SubscribeParams, Subscribed, Tail, TailParams, Until, WaitParams, WaitReason, Waited,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

const DETACH_KEY: &str = "C-\\";

/// The session a command inside a session defaults to.
const SESSION_ENV: &str = "CLANKERMUX_SESSION";
/// The socket of the server that holds it.
const SOCKET_ENV: &str = "CLANKERMUX_SOCKET";

/// Exit codes besides 0, 1 and clap's 2.
const EXIT_USAGE: i32 = 2;
const EXIT_TARGET: i32 = 3;
const EXIT_UNREACHABLE: i32 = 4;
/// A wait that timed out, as `timeout(1)` exits.
const EXIT_TIMEOUT: i32 = 124;
/// A wait interrupted with Ctrl-C and cancelled.
const EXIT_INTERRUPTED: i32 = 130;

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
    /// Print the guide for agents, as `skill show` does.
    #[arg(long, conflicts_with = "stdio")]
    skill: bool,
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
        #[arg(long, conflicts_with = "wait")]
        attach: bool,
        /// Wait for it to exit, from its first byte, and exit with its
        /// status.
        #[arg(long)]
        wait: bool,
        /// With `--wait`: give up after DUR (`500ms`, `10s`, `2m`).
        #[arg(long, value_name = "DUR", value_parser = parse_duration, requires = "wait")]
        timeout: Option<std::time::Duration>,
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
    /// Set the size (`COLSxROWS`, or `COLSx` or `xROWS` to keep the
    /// other), the size policy, or both.
    Resize {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the
        /// size. With `--policy` alone, one argument is the size when it
        /// reads as one, else the session.
        #[arg(value_name = "[SESSION] [SIZE]", num_args = 0..=2)]
        args: Vec<String>,
        /// `latest`: the most recent viewport report wins; `smallest`: the
        /// smallest viewport among the attachments; `fixed`: only resize
        /// changes the size.
        #[arg(long, value_parser = wire::<SizePolicy>)]
        policy: Option<SizePolicy>,
    },
    /// Close another client's attachment; `inspect` lists them by channel.
    Detach {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the
        /// attachment's channel.
        #[arg(value_name = "[SESSION] CHANNEL", num_args = 1..=2, required = true)]
        args: Vec<String>,
        /// Told to the detached client.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Print the screen, with history lines before it, or another scope.
    Read {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
        /// History lines before the screen, for scope `screen`.
        #[arg(long)]
        scrollback: Option<u32>,
        /// `text`, `vt` (with control sequences) or `html`.
        #[arg(long, value_parser = wire::<ReadFormat>)]
        format: Option<ReadFormat>,
        /// `screen`, `history` (all of it, then the screen), `alternate`
        /// or `last_command` (its output, from shell integration).
        #[arg(long, value_parser = wire::<ReadScope>)]
        scope: Option<ReadScope>,
    },
    /// Print the output stream as plain text, from a stream offset.
    Tail {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
        /// The stream offset to start at; by default the oldest byte kept.
        #[arg(long)]
        after: Option<u64>,
        /// Keep printing what follows until the session's stream ends.
        #[arg(long)]
        follow: bool,
    },
    /// Send text as typed, named keys, or stdin's bytes; optionally wait
    /// for what follows.
    Send {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the text.
        #[arg(value_name = "[SESSION] TEXT", num_args = 0..=2)]
        args: Vec<String>,
        /// Press Enter after the text.
        #[arg(long, conflicts_with_all = ["bytes", "keys"])]
        enter: bool,
        /// Send the text (or stdin's bytes) as a paste: bracketed when the
        /// program asked for it, control bytes made harmless.
        #[arg(long, conflicts_with = "keys")]
        paste: bool,
        /// Keys by name, encoded as the program expects them: `enter`,
        /// `tab`, `esc`, `up`, `f5`, `ctrl+c`, `shift+tab`, `alt+x`, `a`.
        #[arg(long, value_name = "KEY", num_args = 1.., conflicts_with = "bytes")]
        keys: Vec<String>,
        /// Send stdin's bytes as they are.
        #[arg(long)]
        bytes: bool,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Wait for the session to exit, print a match, show a status or a
    /// prompt, or fall quiet.
    Wait {
        /// Id, label, or four or more characters from either end of the
        /// id; `$CLANKERMUX_SESSION` when left out.
        session: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Mark the session's `done` and `error` records seen: those named,
    /// or all of them.
    Ack {
        /// The session, `$CLANKERMUX_SESSION` when left out, then the
        /// record ids.
        #[arg(value_name = "[SESSION [ID...]]")]
        args: Vec<String>,
    },
    /// Print the sessions, then their events as JSON lines, each with its
    /// `seq`, until interrupted.
    Events {
        /// Start after this event, replaying the ones the server kept;
        /// when it kept too few the sessions are printed first, as without.
        #[arg(long, value_name = "SEQ")]
        after: Option<u64>,
        /// The server incarnation `--after` counts in, as `events` prints
        /// it on stderr; the connected server's by default.
        #[arg(long, value_name = "ID", requires = "after")]
        incarnation: Option<String>,
    },
    /// The guide for agents.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// The protocol itself: the server's methods, and requests as they
    /// are.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
}

#[derive(Subcommand)]
enum ApiCommand {
    /// The server's methods: name, class and what each does.
    List,
    /// One method: its params, result, errors and examples.
    Describe { method: String },
    /// Send one request as it is and print its result; no session is
    /// assumed.
    Call {
        method: String,
        /// The params as JSON, `-` for stdin; `{}` when left out.
        params: Option<String>,
    },
}

/// What to wait for; the first to happen ends the wait.
#[derive(Args)]
struct WaitArgs {
    /// Until the session exits; then exit with its status (128 + the
    /// signal's number for a signal).
    #[arg(long = "wait-exit", visible_alias = "exit")]
    exit: bool,
    /// Until its output, escape sequences aside, matches RE (Rust regex).
    #[arg(long = "wait-match", visible_alias = "match", value_name = "RE")]
    matching: Option<String>,
    /// Until a program status record is in one of these states (`idle`,
    /// `working`, `done`, `blocked`, `error`).
    #[arg(
        long = "wait-status",
        visible_alias = "status",
        value_name = "STATE,...",
        value_delimiter = ','
    )]
    status: Vec<String>,
    /// Until the next prompt (OSC 133 A).
    #[arg(long = "wait-prompt", visible_alias = "prompt")]
    prompt: bool,
    /// Until no output for MS milliseconds.
    #[arg(long = "wait-quiet", visible_alias = "quiet", value_name = "MS")]
    quiet: Option<u64>,
    /// Give up after DUR (`500ms`, `10s`, `2m`, or seconds) and exit 124.
    #[arg(long, value_name = "DUR", value_parser = parse_duration)]
    timeout: Option<std::time::Duration>,
}

impl WaitArgs {
    /// The `until` these flags name, if any; a timeout alone is a usage
    /// error.
    fn until(&self) -> Result<Option<Until>, Fail> {
        let until = Until {
            exited: self.exit.then_some(true),
            output: self.matching.clone().map(|regex| OutputMatch {
                text: None,
                regex: Some(regex),
            }),
            status: (!self.status.is_empty()).then(|| StatusMatch {
                states: self.status.clone(),
                record: None,
            }),
            prompt: self.prompt.then_some(true),
            quiet_ms: self.quiet,
        };
        if until == Until::default() {
            return match self.timeout {
                Some(_) => Err(Fail::Usage("--timeout needs something to wait for".into())),
                None => Ok(None),
            };
        }
        Ok(Some(until))
    }

    fn timeout_ms(&self) -> Option<u64> {
        self.timeout.map(|t| t.as_millis() as u64)
    }
}

/// `500ms`, `10s`, `2m`, `1h`, or a number of seconds.
fn parse_duration(spec: &str) -> Result<std::time::Duration, String> {
    let (number, unit) = match spec.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(at) => spec.split_at(at),
        None => (spec, "s"),
    };
    let scale = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return Err(format!("{spec:?} is not a duration: 500ms, 10s, 2m, 1h")),
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| std::time::Duration::from_secs_f64(n * scale))
        .ok_or_else(|| format!("{spec:?} is not a duration: 500ms, 10s, 2m, 1h"))
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Print it: what a session is, the commands, the exit codes, the
    /// rules.
    Show,
}

/// The guide `skill show` prints, built into the binary so it matches it.
const SKILL: &str = include_str!("skill.md");

/// `api call`'s params: JSON as given, or read from stdin for `-`.
fn api_params(params: Option<&str>) -> Result<serde_json::Value, Fail> {
    let text = match params {
        None => return Ok(serde_json::json!({})),
        Some("-") => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
        Some(text) => text.to_string(),
    };
    serde_json::from_str(&text).map_err(|e| Fail::Usage(format!("params are not JSON: {e}")))
}

/// Why the CLI failed, by exit code: a client error (1), a usage error
/// (2), a session argument that names no session or several (3), or a
/// server that cannot be reached (4).
enum Fail {
    Client(Error),
    /// Arguments the command cannot use, reported as given.
    Usage(String),
    Target(String),
    Unreachable(String),
    /// Ctrl-C cancelled a wait.
    Interrupted,
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

fn parse_detach_key(spec: &str) -> Result<u8, Fail> {
    attach::parse_key(spec).ok_or_else(|| {
        Fail::Usage(format!(
            "unusable detach key {spec:?}: use C-<letter> or C-<@ [ \\ ] ^ _ ?>"
        ))
    })
}

fn parse_env(entries: &[String]) -> Result<BTreeMap<String, String>, Fail> {
    entries
        .iter()
        .map(|entry| {
            entry
                .split_once('=')
                .filter(|(name, _)| !name.is_empty())
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .ok_or_else(|| {
                    Fail::Usage(format!("environment entry {entry:?} is not NAME=value"))
                })
        })
        .collect()
}

/// A wire name (`last_command`) as its protocol value; serde's error
/// lists the names there are.
fn wire<T: DeserializeOwned>(name: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(name.into())).map_err(|e| e.to_string())
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

/// The session a command runs inside, when it talks to that session's
/// server.
fn here_session(here: bool) -> Option<String> {
    std::env::var(SESSION_ENV)
        .ok()
        .filter(|v| here && !v.is_empty())
}

/// A session argument as an id; omitted, `$CLANKERMUX_SESSION` when `here`
/// (the command talks to the server in `$CLANKERMUX_SOCKET`). None or
/// several matches fail, listing the candidates.
async fn resolve(conn: &Connection, here: bool, needle: Option<String>) -> Result<String, Fail> {
    let needle = match needle.or_else(|| here_session(here)) {
        Some(needle) => needle,
        None if std::env::var_os(SESSION_ENV).is_some_and(|v| !v.is_empty()) => {
            return Err(Fail::Usage(format!(
                "a session is needed: ${SESSION_ENV} is a session of the server in \
                 ${SOCKET_ENV}, not of this one"
            )));
        }
        None => {
            return Err(Fail::Usage(format!(
                "a session is needed: name one, or run inside a session (${SESSION_ENV})"
            )));
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

/// Columns and rows, either left out.
type Sides = (Option<u16>, Option<u16>);

/// `COLSxROWS`, `COLSx` or `xROWS`.
fn parse_size(spec: &str) -> Result<Sides, Fail> {
    let bad = || Fail::Usage(format!("{spec:?} is not COLSxROWS, COLSx or xROWS"));
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

fn emit_json<T: Serialize>(value: &T) {
    emit(&format!(
        "{}\n",
        serde_json::to_string_pretty(value).unwrap()
    ));
}

fn print<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T)) {
    if json {
        emit_json(value);
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
    let attention = match &s.attention {
        Some(proto::Attention {
            state,
            kind: Some(kind),
            ..
        }) => format!("{state}:{kind}"),
        Some(attention) => attention.state.clone(),
        None => String::new(),
    };
    emit(&format!(
        "{:<6} {:<12} {:<8}{:<9} {:<18} {:>3}x{:<3} {} {}\n",
        short_id(&s.id),
        s.label,
        s.status.to_string(),
        exit,
        attention,
        s.cols,
        s.rows,
        s.title,
        s.pwd
    ));
}

/// Binds the carriers `serve` was asked for and runs them with the Unix
/// socket until a signal; then parks every running session.
async fn serve(socket: PathBuf, carriers: Carriers) -> Result<(), Fail> {
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
        .map(|o| server::ws::origin(o).map_err(Fail::Usage))
        .collect::<Result<Vec<_>, _>>()?;
    let ws_bind = ws
        .as_deref()
        .map(|bind| match bind.parse::<u16>() {
            Ok(port) => Ok(WsBind::Port(port)),
            Err(_) => match bind.parse::<SocketAddr>() {
                Ok(addr) if addr.ip().is_loopback() => Ok(WsBind::Loopback(addr)),
                _ => Err(Fail::Usage(format!(
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
    // The release, which `WELCOME`, XTVERSION and `describe` report.
    config.version = env!("CARGO_PKG_VERSION").into();
    // Children run elsewhere: they get the socket as an absolute path.
    config.socket = std::path::absolute(&socket)?;
    let registry = server::Registry::new(config)?;
    // One status request serves every carrier that needs the tailnet.
    let needs_tailnet = tcp.is_some()
        || matches!(ws_bind, Some(WsBind::Port(_)))
        || (ws_bind.is_some() && tls_files.is_none());
    let tailnet = if needs_tailnet {
        let api = server::tailscale::LocalApi::discover()
            .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
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
            .ok_or_else(|| Error::Io(std::io::Error::other("this node has no tailnet address")))?;
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
                    Error::Io(std::io::Error::other(
                        "this node has no certificate domain; pass --tls-cert and --tls-key",
                    ))
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
    Ok(result?)
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

/// One `send` request's worth of input.
enum Chunk {
    Text(String),
    Data(Vec<u8>),
    Keys(Vec<String>),
}

/// `text` in pieces whose JSON fits `budget` bytes (at least one
/// character each).
fn text_chunks(text: &str, budget: usize) -> Vec<String> {
    let mut chunks = vec![String::new()];
    let mut size = 0;
    for c in text.chars() {
        let len = serde_json::to_string(&c).map_or(6, |s| s.len() - 2);
        if size + len > budget && size > 0 {
            chunks.push(String::new());
            size = 0;
        }
        chunks.last_mut().unwrap().push(c);
        size += len;
    }
    chunks
}

/// Input as `send` requests, each within the server's control body (with
/// room for the rest of the JSON) and input queue: text in pieces, bytes
/// as base64 pieces. The wait goes with the last, so it is armed before
/// that one's bytes are written; its reply is returned.
async fn send_chunks(
    conn: &Connection,
    session: &str,
    chunks: Vec<Chunk>,
    paste: bool,
    wait: Option<SendWait>,
) -> Result<Option<(i64, client::connection::Reply)>, Error> {
    let limits = &conn.welcome.limits;
    // A paste's brackets add 12 bytes.
    let bracket = if paste { 12 } else { 0 };
    let budget = (limits.control_body as usize)
        .saturating_sub(1024)
        .min((limits.input_queue as usize).saturating_sub(bracket))
        .max(1);
    let mut requests = Vec::new();
    for chunk in chunks {
        match chunk {
            Chunk::Text(text) => {
                for text in text_chunks(&text, budget) {
                    requests.push(SendParams {
                        text: Some(text),
                        paste,
                        ..Default::default()
                    });
                }
            }
            Chunk::Data(data) => {
                let pieces: Vec<&[u8]> = if data.is_empty() {
                    vec![&[]]
                } else {
                    data.chunks((budget / 4 * 3).max(1)).collect()
                };
                for piece in pieces {
                    requests.push(SendParams {
                        data: Some(base64::engine::general_purpose::STANDARD.encode(piece)),
                        paste,
                        ..Default::default()
                    });
                }
            }
            Chunk::Keys(keys) => requests.push(SendParams {
                keys: Some(keys),
                ..Default::default()
            }),
        }
    }
    let last = requests.len() - 1;
    for (i, mut params) in requests.into_iter().enumerate() {
        params.session = session.to_string();
        if i == last && wait.is_some() {
            params.wait = wait;
            return conn.begin("send", params).map(Some);
        }
        conn.request("send", params).await?;
    }
    Ok(None)
}

/// Waits for `reply`; Ctrl-C cancels the request and exits 130.
async fn finish_wait(
    conn: &Connection,
    id: i64,
    reply: client::connection::Reply,
) -> Result<serde_json::Value, Fail> {
    let finished = conn.finish(reply);
    tokio::pin!(finished);
    tokio::select! {
        outcome = &mut finished => Ok(outcome?),
        _ = tokio::signal::ctrl_c() => {
            let _ = conn.request("cancel", CancelParams { id }).await;
            // The cancelled request's error comes first; a wait that had
            // already ended keeps its result.
            match finished.await {
                Err(Error::Server(e)) if e.code == proto::ErrorCode::Cancelled => {
                    Err(Fail::Interrupted)
                }
                outcome => Ok(outcome?),
            }
        }
    }
}

/// The exit status a finished wait means: the child's for `exited` (128
/// plus the signal's number for a signal, 1 with no exit), 124 for a
/// timeout, else 0.
fn wait_status(waited: &Waited) -> i32 {
    match (waited.reason, &waited.exit) {
        (WaitReason::Exited, Some(proto::Exit::Code { code })) => *code,
        (WaitReason::Exited, Some(proto::Exit::Signal { signal })) => {
            128 + server::pty::signal_by_name(signal)
                .map(|s| s.as_raw())
                .or_else(|| signal.parse().ok())
                .unwrap_or(0)
        }
        (WaitReason::Exited, None) => 1,
        (WaitReason::Timeout, _) => EXIT_TIMEOUT,
        _ => 0,
    }
}

/// One line for a finished wait.
fn wait_line(waited: &Waited) -> String {
    let at = waited.at.offset;
    match (&waited.matched, &waited.exit, &waited.record) {
        (Some(m), _, _) => format!("matched {:?} at {}\n", m.text, m.offset),
        (_, Some(proto::Exit::Code { code }), _) => format!("exited {code}\n"),
        (_, Some(proto::Exit::Signal { signal }), _) => format!("exited SIG{signal}\n"),
        (_, _, Some(r)) => format!("status {} {:?}\n", r.state, r.id),
        _ => format!("{} at {at}\n", waited.reason),
    }
}

/// Prints a finished wait (`--json`: the whole reply) and returns its
/// exit status.
fn report_wait(json: bool, reply: &serde_json::Value, waited: &Waited) -> i32 {
    print(json, reply, |_| emit(&wait_line(waited)));
    wait_status(waited)
}

/// A `wait` request, Ctrl-C cancelling it: the reply and its result.
async fn wait_request(
    conn: &Connection,
    params: WaitParams,
) -> Result<(serde_json::Value, Waited), Fail> {
    let (id, reply) = conn.begin("wait", params)?;
    let reply = finish_wait(conn, id, reply).await?;
    let waited: Waited = serde_json::from_value(reply.clone())
        .map_err(|e| Error::Protocol(format!("wait reply: {e}")))?;
    Ok((reply, waited))
}

/// Whether `text` names a session by its full id or its label, the forms
/// a person would mean as a session.
async fn names_session(conn: &Connection, text: &str) -> Result<bool, Error> {
    let lower = text.to_lowercase();
    Ok(all_sessions(conn)
        .await?
        .iter()
        .any(|s| s.id == text || s.label.to_lowercase() == lower))
}

/// `resize`'s session and size. Without a policy the last argument is the
/// size; with one, a lone argument is the size only when it reads as one.
fn resize_args(
    mut args: Vec<String>,
    policy: Option<SizePolicy>,
) -> Result<(Option<String>, Option<Sides>), Fail> {
    let size = match (args.len(), policy) {
        (0, None) => return Err(Fail::Usage("resize takes SIZE, --policy or both".into())),
        (0, Some(_)) => None,
        (1, Some(_)) => match parse_size(&args[0]) {
            Ok(size) => {
                args.clear();
                Some(size)
            }
            Err(_) => None,
        },
        _ => Some(parse_size(&args.pop().expect("an argument"))?),
    };
    Ok((args.pop(), size))
}

/// `detach`'s session and channel.
fn detach_args(args: Vec<String>) -> Result<(Option<String>, u64), Fail> {
    let (session, rest) = target_and(args, 1);
    let channel = rest[0]
        .parse::<u64>()
        .map_err(|_| Fail::Usage(format!("{:?} is not a channel number", rest[0])))?;
    Ok((session, channel))
}

/// Checks a command's arguments before connecting, so a usage error is
/// one whatever the server's state.
fn check_args(command: &Command) -> Result<(), Fail> {
    match command {
        Command::Create {
            env, detach_key, ..
        } => {
            parse_detach_key(detach_key)?;
            parse_env(env)?;
        }
        Command::Attach { detach_key, .. } => {
            parse_detach_key(detach_key)?;
        }
        Command::Resize { args, policy } => {
            resize_args(args.clone(), *policy)?;
        }
        Command::Detach { args, .. } => {
            detach_args(args.clone())?;
        }
        Command::Send {
            args,
            enter,
            keys,
            bytes,
            wait,
            ..
        } => {
            wait.until()?;
            if let Some(key) = keys
                .iter()
                .find(|k| ghostty_vt::keys::KeyPress::parse(k).is_none())
            {
                return Err(Fail::Usage(format!("unknown key {key:?}")));
            }
            let payload = !keys.is_empty() || *bytes;
            if payload && args.len() > 1 {
                return Err(Fail::Usage("--keys and --bytes take no TEXT".into()));
            }
            if !payload && args.is_empty() && !enter {
                return Err(Fail::Usage(
                    "send takes TEXT, --enter, --keys or --bytes".into(),
                ));
            }
        }
        Command::Api {
            command: ApiCommand::Call { params, .. },
        } if params.as_deref() != Some("-") => {
            api_params(params.as_deref())?;
        }
        Command::Wait { wait, .. } if wait.until()?.is_none() => {
            return Err(Fail::Usage(
                "wait for something: --exit, --match, --status, --prompt or --quiet".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// `tail`, page after page up to where the stream was when it began;
/// with `follow`, waiting at the end for more text or the stream's end,
/// until the stream has ended and the last page is in. With `json`, each page is one line. The loss of what
/// precedes an `after` given is reported once.
async fn tail(
    conn: &Connection,
    session: String,
    after: Option<u64>,
    follow: bool,
    json: bool,
) -> Result<(), Error> {
    let inspect = || {
        conn.call::<_, Session>(
            "inspect",
            SessionParams {
                session: session.clone(),
            },
        )
    };
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let stop_at = if follow {
        None
    } else {
        Some(inspect().await?.offset)
    };
    let warn = after.is_some();
    let mut after = after.unwrap_or(0);
    let mut ended = false;
    let mut first = true;
    loop {
        let request = conn.call::<_, Tail>(
            "tail",
            TailParams {
                session: session.clone(),
                after,
                max_bytes: None,
            },
        );
        let page = tokio::select! {
            page = request => page?,
            _ = &mut ctrl_c => return Ok(()),
        };
        if first && warn && page.truncated {
            eprintln!(
                "clankermux: the stream before offset {} is no longer kept",
                page.start_offset
            );
        }
        if json {
            // A poll that found nothing new is not a page.
            if first || page.next_offset != page.start_offset {
                emit(&format!("{}\n", serde_json::to_string(&page).unwrap()));
            }
        } else {
            emit(&page.text);
        }
        first = false;
        after = page.next_offset;
        if stop_at.is_some_and(|end| after >= end) {
            return Ok(());
        }
        if !page.complete {
            continue;
        }
        if !follow || ended {
            return Ok(());
        }
        // Caught up: wait for a character of text past the page, or the
        // end of the stream, which takes one more page.
        let wait = conn.call::<_, Waited>(
            "wait",
            WaitParams {
                session: session.clone(),
                until: Until {
                    exited: Some(true),
                    output: Some(OutputMatch {
                        text: None,
                        regex: Some("(?s).".into()),
                    }),
                    ..Default::default()
                },
                after: Some(CursorParams {
                    offset: after,
                    marker: 0,
                }),
                timeout_ms: None,
            },
        );
        let waited = tokio::select! {
            waited = wait => waited?,
            _ = &mut ctrl_c => return Ok(()),
        };
        ended = waited.reason == WaitReason::Exited;
    }
}

/// `api`: the catalog the server answers to `describe`, and raw calls.
async fn api(conn: &Connection, command: ApiCommand, json: bool) -> Result<i32, Fail> {
    match command {
        ApiCommand::List => {
            let catalog: Catalog = conn.call("describe", DescribeParams::default()).await?;
            let methods: Vec<serde_json::Value> = catalog
                .methods
                .iter()
                .map(
                    |m| serde_json::json!({"name": m.name, "class": m.class, "summary": m.summary}),
                )
                .collect();
            print(json, &methods, |_| {
                let width = catalog.methods.iter().map(|m| m.name.len()).max();
                for m in &catalog.methods {
                    emit(&format!(
                        "{:<width$}  {:<8}  {}\n",
                        m.name,
                        m.class.to_string(),
                        m.summary,
                        width = width.unwrap_or(0)
                    ));
                }
            });
        }
        ApiCommand::Describe { method } => {
            let params = DescribeParams {
                method: Some(method),
            };
            let one: Description = conn.call("describe", params).await?;
            print(json, &one, |one| emit(&proto::render::method(one)));
        }
        ApiCommand::Call { method, params } => {
            let params = api_params(params.as_deref())?;
            // As `wait`: Ctrl-C cancels a request that waits.
            let (id, reply) = conn.begin(&method, params)?;
            let result = finish_wait(conn, id, reply)
                .await
                .map_err(|fail| match fail {
                    Fail::Client(Error::Server(e)) if e.code == proto::ErrorCode::NoSuchSession => {
                        Fail::Target(format!("{}: {}", e.code, e.message))
                    }
                    other => other,
                })?;
            emit_json(&result);
        }
    }
    Ok(0)
}

async fn run(cli: Cli) -> Result<i32, Fail> {
    let json = cli.json;
    if cli.skill
        || matches!(
            cli.command,
            Some(Command::Skill {
                command: SkillCommand::Show
            })
        )
    {
        emit(SKILL);
        return Ok(0);
    }
    let command = match (cli.stdio, cli.command) {
        (true, None) => None,
        (true, Some(_)) => return Err(Fail::Usage("--stdio takes no command".into())),
        (false, None) => {
            return Err(Fail::Usage("a command is required; see --help".into()));
        }
        (false, Some(command)) => Some(command),
    };
    if let Some(Command::Serve(carriers)) = command {
        if cli.connect.is_some() || cli.ssh.is_some() {
            return Err(Fail::Usage(
                "serve runs locally; --connect and --ssh do not apply".into(),
            ));
        }
        let socket = cli.socket.unwrap_or_else(server::socket_path);
        serve(socket, carriers).await?;
        return Ok(0);
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
    check_args(&command)?;
    // `$CLANKERMUX_SESSION` names a session of the server in
    // `$CLANKERMUX_SOCKET`, and of no other.
    let here = cli.connect.is_none()
        && cli.ssh.is_none()
        && std::env::var_os(SOCKET_ENV).is_some_and(|env| env == socket.as_os_str());
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
        Command::Serve(_) | Command::Skill { .. } => unreachable!(),
        Command::Api { command } => return api(&conn, command, json).await,
        Command::Create {
            label,
            cwd,
            cols,
            rows,
            keep,
            env,
            attach,
            wait,
            timeout,
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
                return Ok(code);
            }
            if wait {
                // From the first byte, so an exit before this request
                // still counts.
                let (reply, waited) = wait_request(
                    &conn,
                    WaitParams {
                        session: created.id.clone(),
                        until: Until {
                            exited: Some(true),
                            ..Default::default()
                        },
                        after: Some(CursorParams {
                            offset: 0,
                            marker: 0,
                        }),
                        timeout_ms: timeout.map(|t| t.as_millis() as u64),
                    },
                )
                .await?;
                let both = serde_json::json!({"session": created, "wait": reply});
                return Ok(report_wait(json, &both, &waited));
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
            let session = resolve(&conn, here, session).await?;
            let value = conn.request("inspect", SessionParams { session }).await?;
            emit_json(&value);
        }
        Command::Attach {
            session,
            read_only,
            detach_key,
        } => {
            let detach_key = parse_detach_key(&detach_key)?;
            let session = resolve(&conn, here, session).await?;
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
            return Ok(code);
        }
        Command::End { session } => {
            let session = resolve(&conn, here, session).await?;
            conn.request("end", SessionParams { session }).await?;
        }
        Command::Signal { args } => {
            let (session, mut rest) = target_and(args, 1);
            let session = resolve(&conn, here, session).await?;
            let signal = rest.remove(0);
            conn.request("signal", SignalParams { session, signal })
                .await?;
        }
        Command::Remove { session } => {
            let session = resolve(&conn, here, session).await?;
            conn.request("remove", SessionParams { session }).await?;
        }
        Command::Label { args } => {
            let (session, mut rest) = target_and(args, 1);
            let session = resolve(&conn, here, session).await?;
            let label = rest.remove(0);
            let updated: Session = conn.call("label", LabelParams { session, label }).await?;
            print(json, &updated, |_| {});
        }
        Command::Resize { args, policy } => {
            let (session, size) = resize_args(args, policy)?;
            let session = resolve(&conn, here, session).await?;
            let (cols, rows) = size.unwrap_or_default();
            conn.request(
                "resize",
                ResizeParams {
                    session,
                    cols,
                    rows,
                    policy,
                },
            )
            .await?;
        }
        Command::Detach { args, reason } => {
            let (session, channel) = detach_args(args)?;
            let session = resolve(&conn, here, session).await?;
            conn.request(
                "detach",
                DetachParams {
                    session,
                    channel,
                    reason,
                },
            )
            .await?;
        }
        Command::Read {
            session,
            scrollback,
            format,
            scope,
        } => {
            let session = resolve(&conn, here, session).await?;
            let read: Read = conn
                .call(
                    "read",
                    ReadParams {
                        session,
                        scrollback,
                        format,
                        scope,
                    },
                )
                .await?;
            print(json, &read, |read| emit(&read.text));
        }
        Command::Tail {
            session,
            after,
            follow,
        } => {
            let session = resolve(&conn, here, session).await?;
            tail(&conn, session, after, follow, json).await?;
        }
        Command::Send {
            args,
            enter,
            paste,
            keys,
            bytes,
            wait,
        } => {
            let until = wait.until()?;
            let payload = !keys.is_empty() || bytes;
            let (session, rest) = target_and(args, if payload { 0 } else { 1 });
            // `send build --enter` means the session `build`, not the text.
            if let (None, [text]) = (&session, rest.as_slice())
                && names_session(&conn, text).await?
            {
                return Err(Fail::Usage(format!(
                    "{text:?} names a session: give SESSION and TEXT, or --keys enter"
                )));
            }
            let mut chunks = match rest.into_iter().next() {
                _ if bytes => {
                    let mut data = Vec::new();
                    std::io::stdin().read_to_end(&mut data)?;
                    vec![Chunk::Data(data)]
                }
                _ if !keys.is_empty() => vec![Chunk::Keys(keys)],
                Some(text) => vec![Chunk::Text(text)],
                // `--enter` alone: check_args refused a send of nothing.
                None => Vec::new(),
            };
            if enter {
                // Enter after a paste, not inside it.
                match chunks.last_mut() {
                    Some(Chunk::Text(text)) if !paste => text.push('\r'),
                    _ => chunks.push(Chunk::Keys(vec!["enter".into()])),
                }
            }
            let session = resolve(&conn, here, session).await?;
            let wait = until.map(|until| SendWait {
                until,
                timeout_ms: wait.timeout_ms(),
            });
            if let Some((id, reply)) = send_chunks(&conn, &session, chunks, paste, wait).await? {
                let reply = finish_wait(&conn, id, reply).await?;
                let sent: Sent = serde_json::from_value(reply.clone())
                    .map_err(|e| Error::Protocol(format!("send reply: {e}")))?;
                let waited = sent
                    .wait
                    .ok_or_else(|| Error::Protocol("send reply without its wait".into()))?;
                return Ok(report_wait(json, &reply, &waited));
            }
        }
        Command::Wait { session, wait } => {
            let until = wait
                .until()?
                .expect("check_args refused a wait for nothing");
            let session = resolve(&conn, here, session).await?;
            let (reply, waited) = wait_request(
                &conn,
                WaitParams {
                    session,
                    until,
                    after: None,
                    timeout_ms: wait.timeout_ms(),
                },
            )
            .await?;
            return Ok(report_wait(json, &reply, &waited));
        }
        Command::Ack { args } => {
            let mut args = args.into_iter();
            let session = resolve(&conn, here, args.next()).await?;
            let ids: Vec<String> = args.collect();
            let acked: Acked = conn
                .call(
                    "ack",
                    AckParams {
                        session,
                        record_ids: (!ids.is_empty()).then_some(ids),
                    },
                )
                .await?;
            print(json, &acked, |_| {});
        }
        Command::Events { after, incarnation } => {
            let mut events = conn.events();
            let params = SubscribeParams {
                after,
                incarnation: after
                    .map(|_| incarnation.unwrap_or_else(|| conn.welcome.incarnation.clone())),
            };
            let subscribed: Subscribed = conn.call("subscribe", params).await?;
            // What a later `--after` needs, off the event stream.
            eprintln!(
                "clankermux: incarnation {}, latest event {}",
                conn.welcome.incarnation, subscribed.latest
            );
            let sessions = match subscribed.sessions {
                Some(sessions) => {
                    let page = Page {
                        sessions,
                        next: subscribed.next,
                    };
                    Some(following_pages(&conn, page).await?)
                }
                None if subscribed.gap == Some(true) => {
                    eprintln!(
                        "clankermux: events after {} are gone; the sessions follow",
                        after.unwrap_or_default()
                    );
                    Some(all_sessions(&conn).await?)
                }
                None => None,
            };
            if let Some(sessions) = sessions {
                print(json, &sessions, |sessions| {
                    sessions.iter().for_each(session_line)
                });
            }
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
                Fail::Usage(message) => (message, EXIT_USAGE),
                Fail::Client(err) => (err.to_string(), 1),
                Fail::Target(message) => (message, EXIT_TARGET),
                Fail::Unreachable(message) => (message, EXIT_UNREACHABLE),
                Fail::Interrupted => ("cancelled".into(), EXIT_INTERRUPTED),
            };
            eprintln!("clankermux: {message}");
            code
        }
    };
    // The stdin reader thread of an attach must not keep the process up.
    std::process::exit(code)
}
