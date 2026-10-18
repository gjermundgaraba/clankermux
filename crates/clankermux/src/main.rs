use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "clankermux terminal multiplexer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Unix socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::Serve { socket } => {
            let socket = socket.unwrap_or_else(server::socket_path);
            let registry = server::Registry::new(server::default_config());
            let result = tokio::select! {
                result = server::serve_unix(socket, registry) => result,
                _ = tokio::signal::ctrl_c() => Ok(()),
            };
            match result {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("clankermux: {err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}
