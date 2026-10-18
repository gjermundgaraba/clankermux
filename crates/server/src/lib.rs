//! The clankermux server.

pub mod connection;
pub mod control;
pub mod pty;
pub mod registry;
pub mod session;
pub mod stream;

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

/// Binds the Unix socket (directory 0700, socket 0600) and serves
/// connections from the server's own uid until the task is cancelled.
pub async fn serve_unix(socket: PathBuf, registry: Arc<Registry>) -> io::Result<()> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let uid = rustix::process::getuid().as_raw();
    loop {
        let (stream, _) = listener.accept().await?;
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == uid => {
                tokio::spawn(connection::serve_connection(stream, registry.clone()));
            }
            _ => drop(stream),
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

pub fn default_config() -> Config {
    Config {
        limits: proto::Limits::default(),
        shell: vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())],
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/")),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}
