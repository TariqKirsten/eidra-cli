//! `eidra relay`: forwards a Unix socket to the model server, so that an agent in a container with
//! no network can still reach the model, and nothing else (docs/DESIGN.md §13).
//!
//! The relay only copies bytes. Whatever connects to the socket talks to the server's port, and to
//! nothing beyond it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use eidra_core::Profile;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpStream, UnixListener};

#[derive(Debug, Args)]
pub struct RelayArgs {
    /// The socket to listen on. Mount it into the agent's container and point
    /// EIDRA_MODEL_SOCKET at it
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,

    /// The server, as host:port [default: the profile's base_url]
    #[arg(long, value_name = "HOST:PORT")]
    to: Option<String>,

    /// A profile directory [default: the built-in profile]
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,
}

pub async fn run(args: RelayArgs) -> Result<()> {
    let target = match args.to {
        Some(target) => target,
        None => {
            let profile = match &args.profile {
                Some(dir) => Profile::load(dir)?,
                None => Profile::builtin()?,
            };
            target_of(&profile.base_url)?
        }
    };
    eprintln!("relaying {} to {target}", args.socket.display());
    serve(&args.socket, &target).await
}

/// `host:port` from a base URL such as `http://127.0.0.1:8000/v1`.
pub fn target_of(base_url: &str) -> Result<String> {
    let Some(rest) = base_url.strip_prefix("http://") else {
        bail!("{base_url}: only plain http:// servers can be relayed");
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        bail!("{base_url} names no server");
    }
    // A port follows the last colon, unless that colon is inside an IPv6 address's brackets.
    let has_port = authority.rsplit_once(':').is_some_and(|(_, port)| !port.contains(']'));
    Ok(if has_port { authority.to_owned() } else { format!("{authority}:80") })
}

/// Listens on `socket`, replacing any socket left there, and forwards every connection to `target`
/// until the future is dropped.
pub async fn serve(socket: &Path, target: &str) -> Result<()> {
    forward(bind(socket)?, target.to_owned()).await
}

/// The listening socket, bound now so that a bad path fails before anything relies on it.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    let _ = std::fs::remove_file(socket);
    UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))
}

/// Forwards every connection `listener` accepts to `target`, until the future is dropped.
pub async fn forward(listener: UnixListener, target: String) -> Result<()> {
    loop {
        let (mut inbound, _) = listener.accept().await.context("accepting a connection")?;
        let target = target.clone();
        tokio::spawn(async move {
            // A refused connection closes the socket's side, and the client reports it.
            if let Ok(mut outbound) = TcpStream::connect(&target).await {
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixStream};

    #[test]
    fn targets_come_from_base_urls() {
        assert_eq!(target_of("http://127.0.0.1:8000/v1").unwrap(), "127.0.0.1:8000");
        assert_eq!(target_of("http://radiance/v1").unwrap(), "radiance:80");
        assert_eq!(target_of("http://[::1]:8000/v1").unwrap(), "[::1]:8000");
        assert_eq!(target_of("http://[::1]/v1").unwrap(), "[::1]:80");
        assert!(target_of("https://example.com/v1").is_err());
        assert!(target_of("http:///v1").is_err());
    }

    #[tokio::test]
    async fn forwards_a_socket_to_a_port() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = server.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut conn, _) = server.accept().await.unwrap();
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).await.unwrap();
            conn.write_all(&buf).await.unwrap();
        });
        let socket =
            std::env::temp_dir().join(format!("eidra-relay-test-{}.sock", std::process::id()));
        let relay = tokio::spawn({
            let socket = socket.clone();
            async move { serve(&socket, &target).await }
        });
        // The relay binds before its first await, but give it a moment to be scheduled.
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        let mut client = UnixStream::connect(&socket).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        relay.abort();
        let _ = std::fs::remove_file(&socket);
    }
}
