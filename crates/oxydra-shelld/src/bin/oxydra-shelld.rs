use std::path::PathBuf;

use clap::Parser;
use oxydra_shelld::ShellDaemonServer;
use tokio::net::UnixListener;

#[derive(Debug, Parser)]
#[command(
    name = "oxydra-shelld",
    about = "Oxydra shell daemon sidecar",
    version,
    long_version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("OXYDRA_GIT_HASH"), ")")
)]
struct Args {
    #[arg(long = "socket")]
    socket: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Remove stale socket file so bind succeeds.
    if args.socket.exists() {
        std::fs::remove_file(&args.socket)?;
    }

    let listener = UnixListener::bind(&args.socket)?;
    eprintln!(
        "oxydra-shelld listening on {}",
        args.socket.to_string_lossy()
    );

    ShellDaemonServer::default()
        .serve_unix_listener(listener)
        .await?;

    Ok(())
}
