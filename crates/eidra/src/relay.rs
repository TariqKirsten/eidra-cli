//! `eidra relay`: forwards connections to the model server, and to nothing else (docs/DESIGN.md
//! §13). On a Unix socket, it lets an agent in a container with no network still reach the model.
//! With `--listen`, it takes TLS on a TCP port, so that eidra on another machine reaches the model
//! encrypted: the certificate is the relay's own, made on first use, and the other eidra trusts it
//! by the pin the relay prints.
//!
//! The relay only copies bytes. Whatever connects to it talks to the server's port, and to nothing
//! beyond it.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Args;
use eidra_core::Profile;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio_rustls::TlsAcceptor;

/// How long a client may take over the TLS handshake before its connection is dropped.
const HANDSHAKE: Duration = Duration::from_secs(10);

#[derive(Debug, Args)]
pub struct RelayArgs {
    /// The socket to listen on. Mount it into the agent's container and point
    /// EIDRA_MODEL_SOCKET at it
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "listen",
        conflicts_with = "listen"
    )]
    socket: Option<PathBuf>,

    /// Listen on this address instead, such as 0.0.0.0:8443, with TLS, for eidra on another
    /// machine. The certificate is made on first use and kept in ~/.eidra/tls; the relay prints
    /// its pin, for that eidra's --tls-pin
    #[arg(long, value_name = "HOST:PORT")]
    listen: Option<String>,

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
    if let Some(socket) = &args.socket {
        eprintln!("relaying {} to {target}", socket.display());
        return serve(socket, &target).await;
    }
    let listen = args.listen.as_deref().context("--socket or --listen is needed")?;
    let dir = eidra_core::eidra_home(&std::env::current_dir()?).join("tls");
    let (acceptor, pin) = tls_acceptor(&dir)?;
    let listener =
        TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
    eprintln!("relaying TLS on {listen} to {target}, with {}", dir.join("cert.pem").display());
    eprintln!("its pin, for eidra on another machine (--tls-pin or EIDRA_TLS_PIN): {pin}");
    forward_tls(listener, acceptor, target).await
}

/// The relay's TLS, from its certificate and key in `dir`, made there on first use; and the
/// certificate's pin. The certificate is self-signed and names no host: a client trusts it by its
/// pin, at whatever address it reaches the relay.
pub fn tls_acceptor(dir: &Path) -> Result<(TlsAcceptor, String)> {
    let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !cert_path.exists() || !key_path.exists() {
        make_certificate(&cert_path, &key_path)?;
    }
    let cert = CertificateDer::from_pem_file(&cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?;
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .with_context(|| format!("reading {}", key_path.display()))?;
    let pin = eidra_llm::tls::pin_of(&cert);
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("TLS with ring")?
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .with_context(|| format!("the certificate and key in {}", dir.display()))?;
    Ok((TlsAcceptor::from(Arc::new(config)), pin))
}

/// A new self-signed certificate (ECDSA P-256) and its key, the key readable by its owner alone.
fn make_certificate(cert_path: &Path, key_path: &Path) -> Result<()> {
    let dir = cert_path.parent().context("a certificate path names a directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let key = rcgen::KeyPair::generate().context("making a key")?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "eidra relay");
    let cert = params.self_signed(&key).context("making a certificate")?;
    // Made anew with its mode, so that the key is never readable by anyone else.
    let _ = fs::remove_file(key_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(key_path)
        .with_context(|| format!("creating {}", key_path.display()))?;
    file.write_all(key.serialize_pem().as_bytes())?;
    fs::write(cert_path, cert.pem()).with_context(|| format!("writing {}", cert_path.display()))
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

/// Forwards every connection `listener` accepts, once its TLS handshake is done, to `target`, until
/// the future is dropped.
pub async fn forward_tls(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    target: String,
) -> Result<()> {
    accept_tls(listener, acceptor, target, HANDSHAKE).await
}

/// The same, with `handshake` for the time a client has to finish its handshake.
async fn accept_tls(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    target: String,
    handshake: Duration,
) -> Result<()> {
    loop {
        let (inbound, _) = listener.accept().await.context("accepting a connection")?;
        let (acceptor, target) = (acceptor.clone(), target.clone());
        tokio::spawn(async move {
            // A handshake that fails or stalls, a port scan's, say, ends only its own connection.
            let Ok(Ok(mut inbound)) =
                tokio::time::timeout(handshake, acceptor.accept(inbound)).await
            else {
                return;
            };
            if let Ok(mut outbound) = TcpStream::connect(&target).await {
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
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

    /// Whatever an agent in a vacuum asks for, the relay hands it to the one server it was
    /// pointed at: a Host header or an absolute URL names another host, and that host never sees
    /// the request. This is the only way out of a vacuum, so it has to stay a pipe, not a proxy.
    #[tokio::test]
    async fn a_request_for_another_host_still_goes_to_the_server() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = server.local_addr().unwrap().to_string();
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        tokio::spawn({
            let seen = std::sync::Arc::clone(&seen);
            async move {
                loop {
                    let (mut conn, _) = server.accept().await.unwrap();
                    let seen = std::sync::Arc::clone(&seen);
                    tokio::spawn(async move {
                        let mut request = vec![0u8; 1024];
                        let read = conn.read(&mut request).await.unwrap();
                        request.truncate(read);
                        seen.lock().await.push(String::from_utf8_lossy(&request).into_owned());
                        conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await
                            .unwrap();
                    });
                }
            }
        });
        let socket =
            std::env::temp_dir().join(format!("eidra-relay-proxy-{}.sock", std::process::id()));
        let relay = tokio::spawn({
            let socket = socket.clone();
            async move { serve(&socket, &target).await }
        });
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        let mut client = UnixStream::connect(&socket).await.unwrap();
        client
            .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        let mut reply = [0u8; 14];
        client.read_exact(&mut reply).await.unwrap();
        // The answer came from the server the relay was pointed at, whoever the request named.
        assert_eq!(&reply, b"HTTP/1.1 404 N");
        assert_eq!(seen.lock().await.len(), 1);
        relay.abort();
        let _ = std::fs::remove_file(&socket);
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eidra-relay-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A server that answers every request with one model, as `/v1/models` does.
    async fn model_server() -> String {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = server.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = server.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut request = vec![0u8; 4096];
                    let _ = conn.read(&mut request).await;
                    let body = r#"{"data": [{"id": "qwen3.8-flash-next"}]}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = conn.write_all(format!("{head}{body}").as_bytes()).await;
                });
            }
        });
        target
    }

    /// A TLS relay on a port of its own, in front of `target`, with its certificate in `dir`;
    /// its address and pin.
    async fn tls_relay(dir: &Path, target: String) -> (String, String) {
        let (acceptor, pin) = tls_acceptor(dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(forward_tls(listener, acceptor, target));
        (address, pin)
    }

    #[tokio::test]
    async fn eidra_reaches_the_server_through_the_tls_relay_by_its_pin_alone() {
        let dir = scratch("tls");
        let (address, pin) = tls_relay(&dir, model_server().await).await;
        // An address, and a certificate that names no host: the pin is all the trust there is.
        let url = format!("https://{address}/v1");
        let client = eidra_llm::Client::new(&url, Some("key".into()), None, Some(&pin)).unwrap();
        assert_eq!(client.models().await.unwrap(), ["qwen3.8-flash-next"]);

        // Another pin, or none (the system's authorities do not know this certificate): refused.
        let other = eidra_llm::tls::pin_of(b"another certificate");
        let client = eidra_llm::Client::new(&url, None, None, Some(&other)).unwrap();
        let error = format!("{:?}", client.models().await.unwrap_err());
        assert!(error.contains("not the pinned one") && error.contains(&pin), "{error}");
        let client = eidra_llm::Client::new(&url, None, None, None).unwrap();
        assert!(client.models().await.is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_relay_keeps_its_certificate_and_its_key_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("identity");
        let (_, first) = tls_acceptor(&dir).unwrap();
        let (_, again) = tls_acceptor(&dir).unwrap();
        assert_eq!(first, again, "the certificate was made again");
        let mode = fs::metadata(dir.join("key.pem")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A missing key means a new certificate, and so a new pin.
        fs::remove_file(dir.join("key.pem")).unwrap();
        let (_, new) = tls_acceptor(&dir).unwrap();
        assert_ne!(new, first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_client_that_never_finishes_its_handshake_is_let_go() {
        let dir = scratch("stall");
        let (acceptor, _) = tls_acceptor(&dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let target = model_server().await;
        tokio::spawn(accept_tls(listener, acceptor, target, Duration::from_millis(200)));
        // Connected, it says nothing: no ClientHello.
        let mut stalled = TcpStream::connect(address).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), stalled.read(&mut buf)).await;
        assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "the connection is still open: {read:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
