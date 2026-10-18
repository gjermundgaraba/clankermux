//! The clankermux server.

pub mod attach;
pub mod connection;
pub mod control;
pub mod park;
pub mod pty;
pub mod records;
pub mod registry;
pub mod session;
pub mod stream;
pub mod tailscale;
pub mod tls;
pub mod ws;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use proto::FrameType;
use tokio::net::UnixListener;

pub use registry::{Config, Registry};

/// A frame with a JSON payload.
pub fn frame_json<T: serde::Serialize>(frame_type: FrameType, channel: u32, body: &T) -> Vec<u8> {
    proto::encode_frame(
        frame_type,
        channel,
        &serde_json::to_vec(body).expect("serializable"),
    )
}

/// Binds the Unix socket (directories it creates 0700, socket 0600) and
/// serves connections from the server's own uid until the task is
/// cancelled. Refuses to replace a socket another server is listening on.
pub async fn serve_unix(socket: PathBuf, registry: Arc<Registry>) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(dir) = socket.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another server is listening on {}", socket.display()),
        ));
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, PermissionsExt::from_mode(0o600))?;
    let uid = rustix::process::getuid().as_raw();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                // Transient errors (EMFILE, ECONNABORTED) must not stop the
                // server; back off briefly instead.
                tracing::warn!(error = %err, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == uid => {
                tokio::spawn(connection::serve_connection(stream, registry.clone()));
            }
            _ => drop(stream),
        }
    }
}

/// Serves `listener` until cancelled: plaintext frames, `TCP_NODELAY`.
/// Trust is the caller's: the binary binds the tailnet address only.
pub async fn serve_tcp(
    listener: tokio::net::TcpListener,
    registry: Arc<Registry>,
) -> io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                tokio::spawn(connection::serve_connection(stream, registry.clone()));
            }
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

/// `$XDG_STATE_HOME/clankermux`, else `~/.local/state/clankermux`.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("clankermux")
}

/// `$XDG_RUNTIME_DIR/clankermux/sock`, else `sock` in the state directory.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("clankermux/sock"),
        None => state_dir().join("sock"),
    }
}

/// The login shell and home directory of the server's user, from the
/// password database.
fn passwd_entry() -> Option<(String, String)> {
    use std::ffi::CStr;
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: zeroed passwd is a valid out parameter; getpwuid_r fills it
    // with pointers into buf, which outlives the copies below.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pw,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() || pw.pw_shell.is_null() || pw.pw_dir.is_null() {
        return None;
    }
    // SAFETY: both are NUL-terminated strings inside buf.
    let string = |p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    Some((string(pw.pw_shell), string(pw.pw_dir)))
}

/// Defaults from the user's password entry, else the environment.
pub fn default_config() -> Config {
    let (shell, home) = passwd_entry().unwrap_or_default();
    let shell = if shell.is_empty() {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    } else {
        shell
    };
    let home = if home.is_empty() {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
    } else {
        PathBuf::from(home)
    };
    Config {
        limits: proto::Limits::default(),
        shell: vec![shell],
        home,
        version: env!("CARGO_PKG_VERSION").into(),
        state_dir: state_dir(),
    }
}
