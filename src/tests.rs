use std::fs;
use std::future;
use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::net::UdpSocket as StdUdpSocket;
use std::path::PathBuf;
use std::time::Duration;

use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::CertifiedIssuer;
use rcgen::ExtendedKeyUsagePurpose;
use rcgen::IsCa;
use rcgen::KeyPair;
use rcgen::KeyUsagePurpose;
use rustls::crypto::ring;
use rustls::server::WebPkiClientVerifier;
use rustls::version;
use tempfile::TempDir;
use tokio::io as tokio_io;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

use super::*;
use crate::config::Congestion;
use crate::config::Transport;

struct Credentials(TempDir);
impl Credentials {
    fn new() -> Result<Self> {
        let directory = Self(tempfile::tempdir()?);
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate()?)?;
        fs::write(directory.path("ca.pem"), ca.pem())?;
        for (name, purpose) in [
            ("edge", ExtendedKeyUsagePurpose::ServerAuth),
            ("origin", ExtendedKeyUsagePurpose::ClientAuth),
            ("other-origin", ExtendedKeyUsagePurpose::ClientAuth),
            ("backend", ExtendedKeyUsagePurpose::ServerAuth),
            ("client", ExtendedKeyUsagePurpose::ClientAuth),
        ] {
            let mut params = CertificateParams::new(vec![format!("{name}.test")])?;
            params.extended_key_usages = vec![purpose];
            let key = KeyPair::generate()?;
            let certificate = params.signed_by(&key, &ca)?;
            fs::write(directory.path(&format!("{name}.pem")), certificate.pem())?;
            fs::write(directory.path(&format!("{name}.key")), key.serialize_pem())?;
        }
        Ok(directory)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
    fn transport(&self, role: &str, peer: &str) -> Transport {
        Transport {
            ca: self.path("ca.pem"),
            cert: self.path(&format!("{role}.pem")),
            key: self.path(&format!("{role}.key")),
            peer_cert: self.path(&format!("{peer}.pem")),
            congestion: Congestion::Bbr,
            max_connections: 8,
            idle_timeout: Duration::from_secs(3),
            connect_timeout: Duration::from_secs(1),
        }
    }
}

async fn connected(state: &mut watch::Receiver<Option<Connection>>) -> Result<Connection> {
    time::timeout(Duration::from_secs(5), async {
        loop {
            let connection = state.borrow().clone();
            if let Some(conn) = connection {
                return Ok(conn);
            }
            state.changed().await?;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_malformed_prefaces_and_fixed_destination() -> Result<()> {
    let directory = Credentials::new()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let e = Arc::new(Edge {
        listen_tcp: Some("127.0.0.1:0".parse()?),
        listen_udp: None,
        bind: "127.0.0.1:0".parse()?,
        transport: directory.transport("edge", "origin"),
    });
    let server = quic::endpoint(&e.transport, e.bind, Role::Edge, true)?;
    let o = Arc::new(Origin {
        edge: server.local_addr()?,
        target_tcp: Some(listener.local_addr()?),
        target_udp: None,
        proxy_tcp: true,
        proxy_udp: false,
        server_name: "edge.test".into(),
        transport: directory.transport("origin", "edge"),
    });
    let client = quic::endpoint(&o.transport, "127.0.0.1:0".parse()?, Role::Origin, true)?;
    let (state, mut edge_state) = watch::channel(None);
    let mut tasks = JoinSet::new();
    let server_handle = server.clone();
    let edge_config = e.clone();
    tasks.spawn(async move { quic::accept(&server_handle, &edge_config.transport, state).await });
    let origin_conn = time::timeout(
        Duration::from_secs(3),
        client.connect(o.edge, &o.server_name)?,
    )
    .await??;
    let edge_conn = connected(&mut edge_state).await?;
    let duplicate = time::timeout(
        Duration::from_secs(3),
        client.connect(o.edge, &o.server_name)?,
    )
    .await??;
    time::timeout(Duration::from_secs(2), duplicate.closed()).await?;
    assert_eq!(
        edge_state
            .borrow()
            .as_ref()
            .expect("origin connected to edge")
            .stable_id(),
        edge_conn.stable_id()
    );
    assert!(
        time::timeout(Duration::from_millis(100), origin_conn.open_bi())
            .await
            .is_err()
    );
    let (tx, rx) = mpsc::channel(8);
    let incoming = origin_conn.clone();
    tasks.spawn(async move {
        loop {
            let (send, recv) = incoming.accept_bi().await?;
            tx.send(Stream { send, recv }).await?;
        }
    });
    tasks.spawn(origin_tcp(o.clone(), rx));
    let valid = Metadata {
        source: "[2001:db8::9]:1234".parse()?,
        destination: "[2001:db8::10]:443".parse()?,
    };
    for offset in [0, 4, 5, 6, 7] {
        let (send, recv) = edge_conn.open_bi().await?;
        let mut stream = Stream { send, recv };
        let mut bad = valid.encode()?;
        bad[offset] = 255;
        stream.write_all(&bad).await?;
        stream.shutdown().await?;
        let mut b = [0; 1];
        assert!(!matches!(
            time::timeout(Duration::from_secs(2), stream.read(&mut b)).await,
            Ok(Ok(n)) if n > 0
        ));
        assert!(
            time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }
    let (send, recv) = edge_conn.open_bi().await?;
    let mut stream = Stream { send, recv };
    stream.write_all(b"RLY\0\x01").await?;
    stream.shutdown().await?;
    assert!(
        time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    let (send, recv) = edge_conn.open_bi().await?;
    let mut stream = Stream { send, recv };
    stream.write_all(&valid.encode()?).await?;
    stream.write_all(b"opaque").await?;
    let (mut target, _) = time::timeout(Duration::from_secs(2), listener.accept()).await??;
    let expected = valid.proxy_v2()?;
    let mut b = vec![0; expected.len() + 6];
    time::timeout(Duration::from_secs(2), target.read_exact(&mut b)).await??;
    assert_eq!(&b[..expected.len()], &expected);
    assert_eq!(&b[expected.len()..], b"opaque");
    transparent_udp_quic(e, o, edge_state, edge_conn, origin_conn).await?;
    server.close(0u32.into(), b"done");
    client.close(0u32.into(), b"done");
    Ok(())
}

async fn transparent_udp_quic(
    e: Arc<Edge>,
    o: Arc<Origin>,
    edge_state: watch::Receiver<Option<Connection>>,
    edge_conn: Connection,
    origin_conn: Connection,
) -> Result<()> {
    use quinn::crypto::rustls::QuicClientConfig;
    use quinn::crypto::rustls::QuicServerConfig;
    use rustls::pki_types::CertificateDer;

    use crate::quic::certs as cert;
    use crate::quic::key;
    let public = UdpSocket::bind("127.0.0.1:0").await?;
    let public_addr = public.local_addr()?;
    let unused = UdpSocket::bind("127.0.0.1:0").await?;
    let mut udp_config = (*o).clone();
    udp_config.target_udp = Some(unused.local_addr()?);
    let actor = tokio::spawn(udp::origin(origin_conn.clone(), Arc::new(udp_config)));
    let edge_actor = tokio::spawn(udp::edge(public, e.transport.clone(), edge_state));
    for n in [0, 1, 17, 61, 62, 100, 1200] {
        edge_conn.send_datagram(vec![255; n].into())?;
    }
    let mut buffer = [0; 2048];
    assert!(
        time::timeout(Duration::from_millis(100), unused.recv_from(&mut buffer))
            .await
            .is_err()
    );
    // Even an authenticated peer cannot select an unsolicited public reply
    // destination.
    let meta = Metadata {
        source: unused.local_addr()?,
        destination: public_addr,
    };
    let mut forged = b"RUD1".to_vec();
    forged.extend_from_slice(&1u64.to_be_bytes());
    forged.extend_from_slice(&1u16.to_be_bytes());
    forged.extend_from_slice(&0u16.to_be_bytes());
    forged.extend_from_slice(&1000u16.to_be_bytes());
    forged.extend_from_slice(&meta.encode()?);
    forged.push(42);
    origin_conn.send_datagram(forged.into())?;
    assert!(
        time::timeout(Duration::from_millis(100), unused.recv_from(&mut buffer))
            .await
            .is_err()
    );
    actor.abort();
    let _ = actor.await;

    // Inner QUIC endpoints own application TLS keys; the relay sees opaque UDP.
    let application = Credentials::new()?;
    let d = application.0.path();
    let mut roots = rustls::RootCertStore::empty();
    for ca in cert(&d.join("ca.pem"))? {
        roots.add(ca)?;
    }
    let provider = Arc::new(ring::default_provider());
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots.clone()), provider.clone())
            .build()?;
    let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert(&d.join("backend.pem"))?, key(&d.join("backend.key"))?)?;
    server_tls.alpn_protocols = vec![b"inner-test".to_vec()];
    let inner_server = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls)?)),
        "127.0.0.1:0".parse()?,
    )?;
    let mut udp_config = (*o).clone();
    udp_config.target_udp = Some(inner_server.local_addr()?);
    let actor = tokio::spawn(udp::origin(origin_conn, Arc::new(udp_config)));
    let expected_client = cert(&d.join("client.pem"))?;
    let server_task = {
        let endpoint = inner_server.clone();
        tokio::spawn(async move {
            let conn = endpoint
                .accept()
                .await
                .expect("application QUIC client connects")
                .await?;
            let peer = conn
                .peer_identity()
                .expect("mTLS supplies a peer identity")
                .downcast::<Vec<CertificateDer<'static>>>()
                .expect("peer identity is an X.509 certificate chain");
            assert_eq!(peer.first(), expected_client.first());
            let (mut send, mut recv) = conn.accept_bi().await?;
            let data = recv.read_to_end(65536).await?;
            assert_eq!(data, vec![81; 32768]);
            send.write_all(&data).await?;
            send.finish()?;
            conn.closed().await;
            Ok::<_, anyhow::Error>(())
        })
    };
    let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&version::TLS13])?
        .with_root_certificates(roots)
        .with_client_auth_cert(cert(&d.join("client.pem"))?, key(&d.join("client.key"))?)?;
    client_tls.alpn_protocols = vec![b"inner-test".to_vec()];
    let mut inner_client = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    inner_client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(client_tls)?,
    )));
    let conn = time::timeout(
        Duration::from_secs(10),
        inner_client.connect(public_addr, "backend.test")?,
    )
    .await??;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&vec![81; 32768]).await?;
    send.finish()?;
    assert_eq!(
        time::timeout(Duration::from_secs(10), recv.read_to_end(65536)).await??,
        vec![81; 32768]
    );
    conn.close(0u32.into(), b"done");
    time::timeout(Duration::from_secs(5), server_task).await???;
    actor.abort();
    edge_actor.abort();
    inner_client.close(0u32.into(), b"done");
    inner_server.close(0u32.into(), b"done");
    Ok(())
}

async fn configs(d: &Credentials, ipv6: bool) -> Result<(Arc<Edge>, Arc<Origin>, TcpListener)> {
    let local = if ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
    let public = TcpListener::bind(local).await?;
    let quic = UdpSocket::bind(local).await?;
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let e = Edge {
        listen_tcp: Some(public.local_addr()?),
        listen_udp: None,
        bind: quic.local_addr()?,
        transport: d.transport("edge", "origin"),
    };
    let o = Origin {
        edge: e.bind,
        target_tcp: Some(target.local_addr()?),
        target_udp: None,
        proxy_tcp: true,
        proxy_udp: false,
        server_name: "edge.test".into(),
        transport: d.transport("origin", "edge"),
    };
    Ok((Arc::new(e), Arc::new(o), target))
}

async fn echo_backend(listener: TcpListener) -> Result<()> {
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let (mut tcp, _) = incoming?;
                workers.spawn(async move {
                    let mut header = [0; 16];
                    tcp.read_exact(&mut header).await?;
                    assert_eq!(&header[..12], b"\r\n\r\n\0\r\nQUIT\n");
                    let size = u16::from_be_bytes(header[14..16].try_into()?);
                    assert!(matches!(size, 12 | 36));
                    let mut address = vec![0; size as usize];
                    tcp.read_exact(&mut address).await?;
                    tcp.write_all(&header).await?;
                    tcp.write_all(&address).await?;
                    let (mut read, mut write) = tcp.split();
                    tokio_io::copy(&mut read, &mut write).await?;
                    write.shutdown().await?;
                    Ok::<(), anyhow::Error>(())
                });
            },
            Some(result) = workers.join_next(), if !workers.is_empty() => { result??; },
        }
    }
}

async fn echo_at(address: SocketAddr, body: &[u8]) -> Result<()> {
    time::timeout(Duration::from_secs(5), async {
        let mut tcp = TcpStream::connect(address).await?;
        let expected = Metadata {
            source: tcp.local_addr()?,
            destination: address,
        }
        .proxy_v2()?;
        let (mut read, mut write) = tcp.split();
        let sending = async {
            write.write_all(body).await?;
            write.shutdown().await
        };
        let receiving = async {
            let mut response = Vec::new();
            read.read_to_end(&mut response).await?;
            anyhow::ensure!(
                response.len() == expected.len() + body.len(),
                "incomplete reply"
            );
            anyhow::ensure!(
                response[..expected.len()] == expected,
                "incorrect PROXY header"
            );
            anyhow::ensure!(&response[expected.len()..] == body, "incorrect payload");
            Ok::<(), anyhow::Error>(())
        };
        let (sent, received) = tokio::join!(sending, receiving);
        sent?;
        received
    })
    .await?
}

async fn ready(address: SocketAddr) -> Result<()> {
    time::timeout(Duration::from_secs(8), async {
        loop {
            if echo_at(address, b"ready").await.is_ok() {
                return;
            }
            time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_without_proxy_preserves_mtls() -> Result<()> {
    use rustls::pki_types::ServerName;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::TlsConnector;

    let relay = Credentials::new()?;
    let application = Credentials::new()?;
    let mut roots = rustls::RootCertStore::empty();
    for ca in quic::certs(&application.path("ca.pem"))? {
        roots.add(ca)?;
    }
    let provider = Arc::new(ring::default_provider());
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots.clone()), provider.clone())
            .build()?;
    let server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            quic::certs(&application.path("backend.pem"))?,
            quic::key(&application.path("backend.key"))?,
        )?;
    let client_certificates = quic::certs(&application.path("client.pem"))?;
    let client_tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&version::TLS13])?
        .with_root_certificates(roots)
        .with_client_auth_cert(
            client_certificates.clone(),
            quic::key(&application.path("client.key"))?,
        )?;
    let acceptor = TlsAcceptor::from(Arc::new(server_tls));
    let connector = TlsConnector::from(Arc::new(client_tls));
    for ipv6 in [false, true] {
        let (e, mut o, backend) = configs(&relay, ipv6).await?;
        let public = e.listen_tcp.expect("TCP fixture listens");
        Arc::get_mut(&mut o)
            .expect("origin configuration is not shared before startup")
            .proxy_tcp = false;
        let acceptor = acceptor.clone();
        let expected_client = client_certificates.clone();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let (tcp, _) = backend.accept().await?;
            let stream = acceptor.accept(tcp).await?;
            assert_eq!(
                stream.get_ref().1.peer_certificates(),
                Some(expected_client.as_slice()),
                "backend authenticates the application client"
            );
            let (mut read, mut write) = tokio_io::split(stream);
            tokio_io::copy(&mut read, &mut write).await?;
            write.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        });
        tasks.spawn(edge(e.clone(), future::pending()));
        tasks.spawn(origin(o, future::pending()));
        let client = time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(tcp) = TcpStream::connect(public).await
                    && let Ok(stream) = connector
                        .connect(
                            ServerName::try_from("backend.test").expect("fixture name"),
                            tcp,
                        )
                        .await
                {
                    break stream;
                }
                time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await?;
        let body = b"\r\n\r\n\0\r\nQUIT\nopaque application bytes".repeat(2048);
        let (mut read, mut write) = tokio_io::split(client);
        let mut received = Vec::new();
        time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(
                async {
                    write.write_all(&body).await?;
                    write.shutdown().await
                },
                read.read_to_end(&mut received),
            )
        })
        .await??;
        assert_eq!(
            received, body,
            "application bytes arrive without a PROXY prefix"
        );
        time::timeout(Duration::from_secs(5), tasks.join_next())
            .await?
            .expect("backend task finishes")??;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_stack_tcp_and_backhaul() -> Result<()> {
    let directory = Credentials::new()?;
    for edge_ip in ["127.0.0.1", "::1"] {
        let (mut e, mut o, backend) = configs(&directory, false).await?;
        drop(backend);
        let backend = TcpListener::bind("[::1]:0").await?;
        let public = sockets::tcp_listener("[::]:0".parse()?)?;
        let public_addr = public.local_addr()?;
        drop(public);
        let backhaul = sockets::udp_socket("[::]:0".parse()?)?;
        let backhaul_addr = backhaul.local_addr()?;
        drop(backhaul);
        let settings = Arc::get_mut(&mut e).expect("edge configuration is not shared");
        settings.listen_tcp = Some(public_addr);
        settings.bind = backhaul_addr;
        let settings = Arc::get_mut(&mut o).expect("origin configuration is not shared");
        settings.edge = SocketAddr::new(edge_ip.parse()?, backhaul_addr.port());
        settings.target_tcp = Some(backend.local_addr()?);
        let mut tasks = JoinSet::new();
        tasks.spawn(echo_backend(backend));
        tasks.spawn(edge(e, future::pending()));
        tasks.spawn(origin(o, future::pending()));
        for client_ip in ["127.0.0.1", "::1"] {
            let address = SocketAddr::new(client_ip.parse()?, public_addr.port());
            ready(address).await?;
            echo_at(address, &vec![73; 65536]).await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcard_udp_preserves_destination_and_reply_source() -> Result<()> {
    let directory = Credentials::new()?;
    for bind in ["0.0.0.0:0", "[::]:0"] {
        for proxy in [false, true] {
            let (mut e, mut o, backend) = configs(&directory, false).await?;
            let tcp_public = e.listen_tcp.expect("TCP fixture listens");
            let public = sockets::udp_socket(bind.parse()?)?;
            let public_addr = public.local_addr()?;
            drop(public);
            Arc::get_mut(&mut e)
                .expect("edge configuration is not shared")
                .listen_udp = Some(public_addr);
            let target = UdpSocket::bind(if proxy { "[::1]:0" } else { "127.0.0.1:0" }).await?;
            let settings = Arc::get_mut(&mut o).expect("origin configuration is not shared");
            settings.target_udp = Some(target.local_addr()?);
            settings.proxy_udp = proxy;
            let mut tasks = JoinSet::new();
            tasks.spawn(async move {
                let mut buffer = vec![0; udp::MAX_PACKET + 1];
                loop {
                    let (n, peer) = target.recv_from(&mut buffer).await?;
                    target.send_to(&buffer[..n], peer).await?;
                }
            });
            tasks.spawn(echo_backend(backend));
            tasks.spawn(edge(e.clone(), future::pending()));
            tasks.spawn(origin(o, future::pending()));
            ready(tcp_public).await?;
            let client = UdpSocket::bind("127.0.0.1:0").await?;
            let mut destinations = vec!["127.0.0.1"];
            #[cfg(target_os = "linux")]
            destinations.push("127.0.0.2");
            if public_addr.is_ipv6() {
                destinations.push("::1");
            }
            for ip in destinations {
                let ipv6_client;
                let sender = if ip == "::1" {
                    ipv6_client = UdpSocket::bind("[::1]:0").await?;
                    &ipv6_client
                } else {
                    &client
                };
                let destination = SocketAddr::new(ip.parse()?, public_addr.port());
                sender.connect(destination).await?;
                let header = Metadata {
                    source: sender.local_addr()?,
                    destination,
                }
                .proxy_v2_datagram()?;
                for size in [0, 1, 4096, 60000] {
                    let body = vec![93; size];
                    sender.send(&body).await?;
                    let mut response = vec![0; udp::MAX_PACKET + 1];
                    let (n, source) =
                        time::timeout(Duration::from_secs(3), sender.recv_from(&mut response))
                            .await??;
                    assert_eq!(
                        source, destination,
                        "reply uses the original destination IP"
                    );
                    let expected = if proxy {
                        [header.as_slice(), body.as_slice()].concat()
                    } else {
                        body
                    };
                    assert_eq!(
                        &response[..n],
                        expected,
                        "payload and PROXY metadata preserved"
                    );
                }
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarding_concurrency_and_restarts() -> Result<()> {
    let d = Credentials::new()?;
    for (ipv6, congestion) in [(false, Congestion::Cubic), (true, Congestion::Bbr)] {
        let (mut e, mut o, backend) = configs(&d, ipv6).await?;
        let public = e.listen_tcp.expect("TCP fixture listens");
        Arc::get_mut(&mut e)
            .expect("edge configuration is not shared before startup")
            .transport
            .max_connections = 64;
        Arc::get_mut(&mut o)
            .expect("origin configuration is not shared before startup")
            .transport
            .max_connections = 64;
        Arc::get_mut(&mut e)
            .expect("edge configuration is not shared before startup")
            .transport
            .congestion = congestion;
        Arc::get_mut(&mut o)
            .expect("origin configuration is not shared before startup")
            .transport
            .congestion = congestion;
        let mut tasks = JoinSet::new();
        tasks.spawn(echo_backend(backend));
        // The origin must tolerate an edge that starts later.
        let (origin_stop, until) = oneshot::channel();
        tasks.spawn(origin(o.clone(), async move {
            until.await?;
            Ok(())
        }));
        time::sleep(Duration::from_millis(100)).await;
        let (edge_stop, until) = oneshot::channel();
        tasks.spawn(edge(e.clone(), async move {
            until.await?;
            Ok(())
        }));
        ready(public).await?;
        echo_at(public, &vec![91; 8 * 1024 * 1024]).await?;
        echo_at(public, b"\r\n\r\n\0\r\nQUIT\nclient-supplied header").await?;
        let mut clients = JoinSet::new();
        for _ in 0..32 {
            let addr = public;
            clients.spawn(async move { echo_at(addr, b"concurrent").await });
        }
        while let Some(result) = clients.join_next().await {
            result??;
        }
        edge_stop.send(()).expect("edge shutdown receiver is alive");
        tasks.join_next().await.expect("relay task is running")??;
        // Unlike a process restart, this test keeps the runtime alive. Quinn's
        // detached draining driver can briefly retain the old UDP socket after
        // edge() returns; wait for its release before rebinding the same port.
        time::timeout(Duration::from_secs(5), async {
            loop {
                match StdUdpSocket::bind(e.bind) {
                    Ok(socket) => {
                        drop(socket);
                        return Ok::<(), io::Error>(());
                    }
                    Err(error) if error.kind() == ErrorKind::AddrInUse => {
                        time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await??;
        tasks.spawn(edge(e.clone(), future::pending()));
        ready(public).await?;
        origin_stop
            .send(())
            .expect("origin shutdown receiver is alive");
        tasks.join_next().await.expect("relay task is running")??;
        tasks.spawn(origin(o, future::pending()));
        ready(public).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_wrong_ca_pin_and_server_name() -> Result<()> {
    let directory = Credentials::new()?;
    let foreign = Credentials::new()?;
    for case in ["client-pin", "server-pin", "ca", "name", "usage"] {
        let (edge_config, mut origin_config, target) = configs(&directory, false).await?;
        let settings = Arc::get_mut(&mut origin_config)
            .expect("origin configuration is not shared before startup");
        match case {
            "client-pin" => {
                settings.transport.cert = directory.path("other-origin.pem");
                settings.transport.key = directory.path("other-origin.key");
            }
            "server-pin" => settings.transport.peer_cert = directory.path("backend.pem"),
            "ca" => {
                settings.transport.cert = foreign.path("origin.pem");
                settings.transport.key = foreign.path("origin.key");
            }
            "name" => settings.server_name = "wrong.test".into(),
            "usage" => {
                settings.transport.cert = directory.path("backend.pem");
                settings.transport.key = directory.path("backend.key");
            }
            _ => unreachable!(),
        }
        let mut tasks = JoinSet::new();
        tasks.spawn(edge(edge_config.clone(), future::pending()));
        tasks.spawn(origin(origin_config, future::pending()));
        time::sleep(Duration::from_millis(1300)).await;
        let mut tcp =
            TcpStream::connect(edge_config.listen_tcp.expect("TCP fixture listens")).await?;
        tcp.write_all(b"must not reach target").await?;
        let mut b = [0; 1];
        assert!(
            !matches!(
                time::timeout(Duration::from_secs(2), tcp.read(&mut b)).await,
                Ok(Ok(n)) if n > 0
            ),
            "accepted {case}"
        );
        assert!(
            time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err(),
            "dialed target for {case}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_limits_and_idle_cleanup() -> Result<()> {
    let d = Credentials::new()?;
    let (mut e, mut o, backend) = configs(&d, false).await?;
    let public = e.listen_tcp.expect("TCP fixture listens");
    for t in [
        &mut Arc::get_mut(&mut e)
            .expect("edge configuration is not shared before startup")
            .transport,
        &mut Arc::get_mut(&mut o)
            .expect("origin configuration is not shared before startup")
            .transport,
    ] {
        t.max_connections = 2;
        t.idle_timeout = Duration::from_secs(1);
    }
    let mut tasks = JoinSet::new();
    tasks.spawn(echo_backend(backend));
    tasks.spawn(edge(e.clone(), future::pending()));
    tasks.spawn(origin(o, future::pending()));
    ready(public).await?;
    time::sleep(Duration::from_millis(100)).await;
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut tcp = TcpStream::connect(public).await?;
        tcp.write_all(b"hold").await?;
        let mut b = [0; 32];
        time::timeout(Duration::from_secs(2), tcp.read_exact(&mut b)).await??;
        held.push(tcp);
    }
    let mut excess = TcpStream::connect(public).await?;
    let mut b = [0; 1];
    assert_eq!(
        time::timeout(Duration::from_secs(1), excess.read(&mut b)).await??,
        0
    );
    for mut tcp in held {
        assert_eq!(
            time::timeout(Duration::from_secs(3), tcp.read(&mut b)).await??,
            0
        );
    }
    ready(public).await?;
    Ok(())
}

#[tokio::test]
async fn relay_reverse_half_close_and_idle_timeout() -> Result<()> {
    let (mut client, a) = tokio_io::duplex(32);
    let (b, mut backend) = tokio_io::duplex(32);
    let mut tasks = JoinSet::new();
    tasks.spawn(forward::relay(a, b, Duration::from_secs(1)));
    backend.write_all(b"reply").await?;
    backend.shutdown().await?;
    let mut received = Vec::new();
    time::timeout(Duration::from_secs(2), client.read_to_end(&mut received)).await??;
    assert_eq!(received, b"reply");
    client.write_all(b"after EOF").await?;
    client.shutdown().await?;
    received.clear();
    backend.read_to_end(&mut received).await?;
    assert_eq!(received, b"after EOF");
    tasks.join_next().await.expect("relay task is running")??;
    let (_client, a) = tokio_io::duplex(32);
    let (b, _backend) = tokio_io::duplex(32);
    assert!(
        time::timeout(
            Duration::from_secs(2),
            forward::relay(a, b, Duration::from_millis(100))
        )
        .await?
        .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_only_and_disabled_tcp_target() -> Result<()> {
    let directory = Credentials::new()?;
    for ipv6 in [false, true] {
        for tcp_ingress in [false, true] {
            let (mut e, mut o, unused_tcp_target) = configs(&directory, ipv6).await?;
            let tcp_public = e.listen_tcp.expect("TCP fixture listens");
            let local = if ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
            let public = UdpSocket::bind(local).await?;
            let public_addr = public.local_addr()?;
            drop(public);
            let settings = Arc::get_mut(&mut e).expect("edge configuration is not shared");
            settings.listen_tcp = tcp_ingress.then_some(tcp_public);
            settings.listen_udp = Some(public_addr);
            let target = UdpSocket::bind("127.0.0.1:0").await?;
            let settings = Arc::get_mut(&mut o).expect("origin configuration is not shared");
            settings.target_tcp = None;
            settings.target_udp = Some(target.local_addr()?);
            settings.proxy_tcp = false;
            let mut tasks = JoinSet::new();
            tasks.spawn(async move {
                let mut buffer = [0; 8192];
                loop {
                    let (n, peer) = target.recv_from(&mut buffer).await?;
                    target.send_to(&buffer[..n], peer).await?;
                }
            });
            tasks.spawn(edge(e, future::pending()));
            tasks.spawn(origin(o, future::pending()));
            time::timeout(Duration::from_secs(5), async {
                let probe = UdpSocket::bind(local).await?;
                let mut buffer = [0; 16];
                loop {
                    probe.send_to(b"ready", public_addr).await?;
                    if let Ok(Ok(n)) =
                        time::timeout(Duration::from_millis(100), probe.recv(&mut buffer)).await
                    {
                        assert_eq!(&buffer[..n], b"ready");
                        return Ok::<(), anyhow::Error>(());
                    }
                }
            })
            .await??;
            if tcp_ingress {
                let mut tcp = TcpStream::connect(tcp_public).await?;
                let mut buffer = [0; 1];
                let result = time::timeout(Duration::from_secs(2), tcp.read(&mut buffer)).await?;
                assert!(
                    !matches!(result, Ok(n) if n > 0),
                    "an unconfigured TCP target resets the stream"
                );
            }
            let client = UdpSocket::bind(local).await?;
            for size in [0, 1, 4096] {
                let body = vec![73; size];
                client.send_to(&body, public_addr).await?;
                let mut buffer = vec![0; size + 1];
                let (n, source) =
                    time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer)).await??;
                assert_eq!(source, public_addr);
                assert_eq!(&buffer[..n], body);
            }
            assert!(
                time::timeout(Duration::from_millis(50), unused_tcp_target.accept())
                    .await
                    .is_err(),
                "UDP-only origins do not dial TCP targets"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_payloads_and_proxy() -> Result<()> {
    let directory = Credentials::new()?;
    for ipv6 in [false, true] {
        let (mut edge_config, mut origin_config, backend) = configs(&directory, ipv6).await?;
        let tcp_public = edge_config.listen_tcp.expect("TCP fixture listens");
        let local = if ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
        let public = UdpSocket::bind(local).await?;
        let public_addr = public.local_addr()?;
        drop(public);
        Arc::get_mut(&mut edge_config)
            .expect("edge configuration is not shared before startup")
            .listen_udp = Some(public_addr);
        let target = UdpSocket::bind("127.0.0.1:0").await?;
        let settings = Arc::get_mut(&mut origin_config)
            .expect("origin configuration is not shared before startup");
        settings.target_udp = Some(target.local_addr()?);
        settings.proxy_udp = ipv6;
        let client = UdpSocket::bind(local).await?;
        let expected = Metadata {
            source: client.local_addr()?,
            destination: public_addr,
        }
        .proxy_v2_datagram()?;
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut b = vec![0; udp::MAX_PACKET + 1];
            loop {
                let (n, source) = target.recv_from(&mut b).await?;
                let body = if ipv6 {
                    anyhow::ensure!(
                        b[..expected.len()] == expected,
                        "incorrect UDP PROXY header"
                    );
                    &b[expected.len()..n]
                } else {
                    &b[..n]
                };
                target.send_to(body, source).await?;
            }
        });
        tasks.spawn(echo_backend(backend));
        tasks.spawn(edge(edge_config.clone(), future::pending()));
        tasks.spawn(origin(origin_config, future::pending()));
        ready(tcp_public).await?;
        for size in [0, 1, 4096] {
            let body = vec![93; size];
            client.send_to(&body, public_addr).await?;
            let mut b = vec![0; size + 1];
            let (n, source) =
                time::timeout(Duration::from_secs(2), client.recv_from(&mut b)).await??;
            assert_eq!(source, public_addr);
            assert_eq!(&b[..n], body);
        }
    }
    Ok(())
}
