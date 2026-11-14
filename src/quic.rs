use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::result;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;
use quinn::Connection;
use quinn::Endpoint;
use quinn::TransportConfig;
use quinn::congestion::BbrConfig;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::crypto::ring;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::pem::PemObject;
use rustls::server::WebPkiClientVerifier;
use rustls::version;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time;

use crate::config::Congestion;
use crate::config::Origin;
use crate::config::Transport;
use crate::sockets;
use crate::udp;

pub struct Stream {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, b)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

pub fn certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<_> = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("open {}", path.display()))?
        .collect::<result::Result<_, _>>()?;
    ensure!(!certs.is_empty(), "no certificates in {}", path.display());
    Ok(certs)
}

pub fn key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("read private key {}", path.display()))
}

fn authorized(connection: &Connection, expected: &CertificateDer<'static>) -> bool {
    connection
        .peer_identity()
        .and_then(|p| p.downcast::<Vec<CertificateDer<'static>>>().ok())
        .is_some_and(|chain| chain.first() == Some(expected))
}

#[derive(Clone, Copy)]
pub enum Role {
    Edge,
    Origin,
}

pub fn endpoint(c: &Transport, bind: SocketAddr, role: Role, udp: bool) -> Result<Endpoint> {
    let server = matches!(role, Role::Edge);
    let mut roots = rustls::RootCertStore::empty();
    for ca in certs(&c.ca)? {
        roots.add(ca)?;
    }
    let provider = Arc::new(ring::default_provider());
    let mut transport = TransportConfig::default();
    // Only the edge opens streams. No peer may open unidirectional streams.
    transport.max_concurrent_bidi_streams(if server { 0u32 } else { c.max_connections }.into());
    transport.max_concurrent_uni_streams(0u32.into());
    transport.stream_receive_window((4 * 1024 * 1024u32).into());
    transport.receive_window((32 * 1024 * 1024u32).into());
    transport.send_window(32 * 1024 * 1024);
    if matches!(c.congestion, Congestion::Bbr) {
        transport.congestion_controller_factory(Arc::new(BbrConfig::default()));
    }
    transport.datagram_receive_buffer_size(udp.then_some(4 * 1024 * 1024));
    transport.datagram_send_buffer_size(if udp { 4 * 1024 * 1024 } else { 0 });
    transport.max_idle_timeout(Some(c.idle_timeout.try_into()?));
    transport.keep_alive_interval(Some(Duration::from_secs(1).min(c.idle_timeout / 3)));
    let transport = Arc::new(transport);
    let (server_config, client_config) = if server {
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()?;
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&version::TLS13])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs(&c.cert)?, key(&c.key)?)?;
        tls.alpn_protocols = vec![b"zerotun/1".to_vec()];
        tls.max_early_data_size = 0;
        let mut config =
            quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
        config.transport_config(transport);
        config.max_incoming(8);
        (Some(config), None)
    } else {
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&version::TLS13])?
            .with_root_certificates(roots)
            .with_client_auth_cert(certs(&c.cert)?, key(&c.key)?)?;
        tls.alpn_protocols = vec![b"zerotun/1".to_vec()];
        tls.enable_early_data = false;
        let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
        config.transport_config(transport);
        (None, Some(config))
    };
    let socket = sockets::udp_socket(bind).context("bind QUIC socket")?;
    let reference = socket2::SockRef::from(&socket);
    reference.set_recv_buffer_size(2 * 1024 * 1024)?;
    reference.set_send_buffer_size(2 * 1024 * 1024)?;
    eprintln!(
        "QUIC listening {} (socket buffers: receive={}, send={})",
        socket.local_addr()?,
        reference.recv_buffer_size()?,
        reference.send_buffer_size()?
    );
    let mut endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        server_config,
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    if let Some(client) = client_config {
        endpoint.set_default_client_config(client);
    }
    Ok(endpoint)
}

pub async fn accept(
    endpoint: &Endpoint,
    c: &Transport,
    state: watch::Sender<Option<Connection>>,
) -> Result<()> {
    let peer = certs(&c.peer_cert)?.remove(0);
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { return Ok(()); };
                if workers.len() >= 4 {
                    incoming.refuse();
                    continue;
                }
                // Validate the remote address before allocating handshake state.
                if !incoming.remote_address_validated() {
                    let _ = incoming.retry();
                    continue;
                }
                let state = state.clone();
                let peer = peer.clone();
                let deadline = c.connect_timeout;
                workers.spawn(async move {
                    let Ok(Ok(conn)) = time::timeout(deadline, incoming).await else { return; };
                    if !authorized(&conn, &peer) {
                        conn.close(1u32.into(), b"unauthorized identity");
                        return;
                    }
                    let mut accepted = false;
                    state.send_if_modified(|active| {
                        if active.as_ref().is_none_or(|c| c.close_reason().is_some()) {
                            *active = Some(conn.clone());
                            accepted = true;
                            true
                        } else { false }
                    });
                    if !accepted {
                        conn.close(1u32.into(), b"one origin only");
                        return;
                    }
                    eprintln!("backhaul connected");
                    let reason = conn.closed().await;
                    state.send_if_modified(|active| {
                        if active.as_ref().is_some_and(|c| c.stable_id() == conn.stable_id()) {
                            *active = None;
                            true
                        } else { false }
                    });
                    eprintln!("backhaul disconnected: {reason}");
                });
            },
            Some(result) = workers.join_next(), if !workers.is_empty() => { result?; },
        }
    }
}

pub async fn connect(
    endpoint: &Endpoint,
    c: Arc<Origin>,
    streams: mpsc::Sender<Stream>,
) -> Result<()> {
    let peer = certs(&c.transport.peer_cert)?.remove(0);
    // Fail invalid local configuration at startup, rather than retrying it forever.
    ServerName::try_from(c.server_name.as_str())?;
    loop {
        let result: Result<()> = async {
            let conn = time::timeout(
                c.transport.connect_timeout,
                endpoint.connect(c.edge, &c.server_name)?,
            )
            .await??;
            if !authorized(&conn, &peer) {
                conn.close(1u32.into(), b"unauthorized identity");
                anyhow::bail!("edge certificate does not match --peer-cert");
            }
            eprintln!("backhaul connected");
            let mut datagrams = JoinSet::new();
            if c.target_udp.is_some() {
                datagrams.spawn(udp::origin(conn.clone(), c.clone()));
            }
            loop {
                let incoming = tokio::select! {
                    r = conn.accept_bi() => r,
                    _ = datagrams.join_next(), if !datagrams.is_empty() => {
                        conn.close(1u32.into(), b"UDP driver stopped");
                        anyhow::bail!("UDP driver stopped");
                    }
                };
                let (send, recv) = incoming?;
                // Dropping excess streams resets them without growing a task queue.
                let _ = streams.try_send(Stream { send, recv });
            }
        }
        .await;
        if let Err(error) = result {
            eprintln!("backhaul disconnected: {error:#}; retrying in 1s");
        }
        time::sleep(Duration::from_secs(1)).await;
    }
}
