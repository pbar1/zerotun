use std::env;
use std::fs;
use std::fs::File;
use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::net::UdpSocket as StdUdpSocket;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use futures_util::future;
use rustls::pki_types::ServerName;
use tokio::io;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::credentials::Credentials;
use super::link::Link;

pub const CHUNK: usize = 65536;
pub const DEADLINE: Duration = Duration::from_secs(15);
pub static PATTERN: [u8; CHUNK] = {
    let mut data = [0; CHUNK];
    let mut i = 0;
    let mut value = 0;
    while i < CHUNK {
        data[i] = value;
        value = if value == 250 { 0 } else { value + 1 };
        i += 1;
    }
    data
};

pub fn binary() -> PathBuf {
    env::var_os("ZEROTUN_BENCH_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_zerotun")),
        PathBuf::from,
    )
}

pub enum Network {
    Loopback,
    Ipv6,
    Metro,
}

pub struct Fixture {
    credentials: Credentials,
    public: SocketAddr,
    children: Vec<Child>,
    workers: Vec<JoinHandle<()>>,
    link: Option<Link>,
}

fn tcp_port(ipv6: bool) -> Result<SocketAddr> {
    Ok(StdTcpListener::bind(if ipv6 { "[::1]:0" } else { "127.0.0.1:0" })?.local_addr()?)
}

fn udp_port() -> Result<SocketAddr> {
    Ok(StdUdpSocket::bind("127.0.0.1:0")?.local_addr()?)
}

impl Fixture {
    pub async fn start(controller: &str, network: Network) -> Result<Self> {
        let credentials = Credentials::new()?;
        let public = tcp_port(matches!(network, Network::Ipv6))?;
        let edge = udp_port()?;
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        let target = backend.local_addr()?;
        let udp = UdpSocket::bind(target).await?;
        let link = if matches!(network, Network::Metro) {
            Some(Link::start(edge)?)
        } else {
            None
        };
        let remote = link.as_ref().map_or(edge, |p| p.address);
        let server_tls = credentials.server.clone();
        let workers = vec![
            tokio::spawn(async move {
                let mut clients = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = backend.accept() => {
                            let Ok((tcp, _)) = accepted else { return; };
                            let acceptor = TlsAcceptor::from(server_tls.clone());
                            clients.spawn(async move {
                                // Client aborts are expected when an infinite load ends.
                                let _ = serve(tcp, acceptor).await;
                            });
                        },
                        Some(result) = clients.join_next(), if !clients.is_empty() => {
                            result.expect("benchmark backend panicked");
                        },
                    }
                }
            }),
            tokio::spawn(async move {
                let mut b = [0; 65536];
                while let Ok((n, peer)) = udp.recv_from(&mut b).await {
                    if udp.send_to(&b[..n], peer).await.is_err() {
                        return;
                    }
                }
            }),
        ];
        let mut fixture = Self {
            credentials,
            public,
            children: Vec::new(),
            workers,
            link,
        };
        let common = |role: &str, peer: &str| {
            let d = fixture.credentials.directory.path();
            let mut command = Command::new(binary());
            command
                .arg(role)
                .arg("--ca")
                .arg(d.join("ca.pem"))
                .arg("--cert")
                .arg(d.join(format!("{role}.pem")))
                .arg("--key")
                .arg(d.join(format!("{role}.key")))
                .arg("--peer-cert")
                .arg(d.join(format!("{peer}.pem")))
                .args([
                    "--congestion",
                    controller,
                    "--idle-timeout",
                    "120",
                    "--connect-timeout",
                    "3",
                ])
                .stdout(Stdio::null());
            command
        };
        let mut edge_command = common("edge", "origin");
        edge_command
            .arg("--listen-tcp")
            .arg(public.to_string())
            .arg("--listen-udp")
            .arg(public.to_string())
            .arg("--bind")
            .arg(edge.to_string())
            .stderr(File::create(
                fixture.credentials.directory.path().join("edge.log"),
            )?);
        let mut origin_command = common("origin", "edge");
        origin_command
            .arg("--edge")
            .arg(remote.to_string())
            .args(["--server-name", "edge.test"])
            .arg("--target-tcp")
            .arg(target.to_string())
            .arg("--target-udp")
            .arg(target.to_string())
            .arg("--proxy-tcp")
            .stderr(File::create(
                fixture.credentials.directory.path().join("origin.log"),
            )?);
        fixture
            .children
            .push(edge_command.spawn().context("start edge binary")?);
        fixture
            .children
            .push(origin_command.spawn().context("start origin binary")?);
        time::timeout(DEADLINE, async {
            loop {
                fixture.check()?;
                if let Ok(mut client) = fixture.client().await {
                    client.echo(128).await?;
                    client.close().await?;
                    return Ok::<(), anyhow::Error>(());
                }
                time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("wait for authenticated relay readiness")??;
        Ok(fixture)
    }

    pub fn check(&mut self) -> Result<()> {
        for child in &mut self.children {
            if let Some(status) = child.try_wait()? {
                let logs = ["edge", "origin"].map(|role| {
                    fs::read_to_string(
                        self.credentials
                            .directory
                            .path()
                            .join(format!("{role}.log")),
                    )
                    .unwrap_or_default()
                });
                bail!("relay exited {status}: {}\n{}", logs[0], logs[1]);
            }
        }
        ensure!(
            self.workers.iter().all(|p| !p.is_finished()),
            "benchmark backend stopped"
        );
        if let Some(link) = &self.link {
            link.check();
        }
        Ok(())
    }

    pub async fn client(&self) -> Result<Client> {
        time::timeout(DEADLINE, async {
            let tcp = TcpStream::connect(self.public).await?;
            tcp.set_nodelay(true)?;
            let expected = proxy(tcp.local_addr()?, tcp.peer_addr()?);
            let mut stream = TlsConnector::from(self.credentials.client.clone())
                .connect(ServerName::try_from("backend.test")?, tcp)
                .await?;
            ensure!(
                stream.get_ref().1.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
                "application TLS 1.3 required"
            );
            let size = stream.read_u16().await? as usize;
            ensure!(size == expected.len(), "incorrect PROXY header size");
            let mut header = vec![0; size];
            stream.read_exact(&mut header).await?;
            ensure!(header == expected, "incorrect PROXY source/destination");
            Ok(Client {
                stream,
                sequence: 0,
            })
        })
        .await?
    }

    pub async fn clients(&self, count: usize) -> Result<Vec<Client>> {
        future::try_join_all((0..count).map(|_| self.client())).await
    }

    pub async fn udp(&self) -> Result<UdpClient> {
        let socket = UdpSocket::bind(if self.public.is_ipv4() {
            "127.0.0.1:0"
        } else {
            "[::1]:0"
        })
        .await?;
        socket.connect(self.public).await?;
        Ok(UdpClient {
            socket,
            sequence: 0,
            received: [0; 65536],
        })
    }

    pub async fn load(&self) -> Result<Load> {
        let mut tasks = Vec::new();
        let counters = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        for counter in &counters {
            let mut client = self.client().await?;
            client.stream.write_u8(b'F').await?;
            let counter = counter.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    verified(&mut client.stream, CHUNK)
                        .await
                        .expect("background media failed");
                    counter.fetch_add(CHUNK as u64, Ordering::Relaxed);
                }
            }));
        }
        let load = Load { tasks, counters };
        time::timeout(DEADLINE, async {
            while load
                .counters
                .iter()
                .any(|c| c.load(Ordering::Relaxed) < 1024 * 1024)
            {
                load.check();
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(load)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking() {
            for role in ["edge", "origin"] {
                let path = self
                    .credentials
                    .directory
                    .path()
                    .join(format!("{role}.log"));
                if let Ok(log) = fs::read_to_string(path) {
                    eprintln!("{role}: {log}");
                }
            }
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        for task in &self.workers {
            task.abort();
        }
    }
}

pub struct Load {
    tasks: Vec<JoinHandle<()>>,
    counters: [Arc<AtomicU64>; 2],
}
impl Load {
    pub fn progress(&self) -> [u64; 2] {
        self.counters.each_ref().map(|c| c.load(Ordering::Relaxed))
    }

    pub fn check(&self) {
        assert!(
            self.tasks.iter().all(|t| !t.is_finished()),
            "background media stopped"
        );
    }
}
impl Drop for Load {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub struct Client {
    stream: TlsStream<TcpStream>,
    sequence: u64,
}
impl Client {
    pub async fn echo(&mut self, bytes: usize) -> Result<()> {
        let mut message = PATTERN[..bytes].to_vec();
        message[..8].copy_from_slice(&self.sequence.to_be_bytes());
        self.sequence += 1;
        self.stream.write_u8(b'E').await?;
        self.stream.write_u64(bytes as u64).await?;
        self.stream.write_all(&message).await?;
        // TLS may accept plaintext while retaining ciphertext under backpressure.
        // Flush at request/response boundaries before waiting for the peer.
        self.stream.flush().await?;
        let mut response = vec![0; bytes];
        self.stream.read_exact(&mut response).await?;
        ensure!(response == message, "incorrect echo payload");
        Ok(())
    }

    pub async fn download(&mut self, bytes: usize) -> Result<()> {
        self.stream.write_u8(b'D').await?;
        self.stream.write_u64(bytes as u64).await?;
        self.stream.flush().await?;
        verified(&mut self.stream, bytes).await
    }

    pub async fn duplex(&mut self, bytes: usize) -> Result<()> {
        self.stream.write_u8(b'E').await?;
        self.stream.write_u64(bytes as u64).await?;
        let (mut read, mut write) = io::split(&mut self.stream);
        tokio::try_join!(send(&mut write, bytes), verified(&mut read, bytes))?;
        Ok(())
    }

    pub async fn close(mut self) -> Result<()> {
        time::timeout(DEADLINE, async {
            self.stream.write_u8(b'Q').await?;
            self.stream.shutdown().await?;
            let mut rest = Vec::new();
            self.stream.read_to_end(&mut rest).await?;
            ensure!(rest.is_empty(), "unexpected bytes after response");
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }
}

pub struct UdpClient {
    socket: UdpSocket,
    sequence: u64,
    received: [u8; 65536],
}
impl UdpClient {
    pub async fn exchange(&mut self, bytes: usize) -> Result<()> {
        let mut message = PATTERN[..bytes].to_vec();
        message[..8].copy_from_slice(&self.sequence.to_be_bytes());
        self.sequence += 1;
        self.socket.send(&message).await?;
        let n = time::timeout(DEADLINE, self.socket.recv(&mut self.received)).await??;
        ensure!(
            self.received[..n] == message,
            "incorrect UDP payload/boundary"
        );
        Ok(())
    }
}

async fn verified<R: AsyncRead + Unpin>(read: &mut R, bytes: usize) -> Result<()> {
    let mut buffer = [0; CHUNK];
    let mut left = bytes;
    while left != 0 {
        let n = left.min(CHUNK);
        read.read_exact(&mut buffer[..n]).await?;
        ensure!(buffer[..n] == PATTERN[..n], "incorrect download payload");
        left -= n;
    }
    Ok(())
}

async fn send<W: AsyncWrite + Unpin>(write: &mut W, bytes: usize) -> Result<()> {
    let mut left = bytes;
    while left != 0 {
        let n = left.min(CHUNK);
        write.write_all(&PATTERN[..n]).await?;
        left -= n;
    }
    write.flush().await?;
    Ok(())
}

async fn serve(mut tcp: TcpStream, acceptor: TlsAcceptor) -> Result<()> {
    tcp.set_nodelay(true)?;
    let mut header = [0; 16];
    tcp.read_exact(&mut header).await?;
    ensure!(
        header[..13] == *b"\r\n\r\n\0\r\nQUIT\n\x21",
        "missing PROXY v2"
    );
    let size = u16::from_be_bytes([header[14], header[15]]) as usize;
    ensure!(
        matches!((header[13], size), (0x11, 12) | (0x21, 36)),
        "invalid PROXY address family"
    );
    let mut proxy = header.to_vec();
    proxy.resize(16 + size, 0);
    tcp.read_exact(&mut proxy[16..]).await?;
    let mut stream = acceptor.accept(tcp).await?;
    ensure!(
        stream.get_ref().1.peer_certificates().is_some(),
        "application client certificate required"
    );
    stream.write_u16(u16::try_from(proxy.len())?).await?;
    stream.write_all(&proxy).await?;
    stream.flush().await?;
    loop {
        match stream.read_u8().await? {
            b'E' => {
                let mut left = usize::try_from(stream.read_u64().await?)?;
                ensure!(left <= 32 * 1024 * 1024, "oversized echo");
                let mut buffer = [0; CHUNK];
                while left != 0 {
                    let n = left.min(CHUNK);
                    stream.read_exact(&mut buffer[..n]).await?;
                    stream.write_all(&buffer[..n]).await?;
                    left -= n;
                }
                stream.flush().await?;
            }
            b'D' => {
                let n = usize::try_from(stream.read_u64().await?)?;
                ensure!(n <= 32 * 1024 * 1024, "oversized download");
                send(&mut stream, n).await?;
            }
            b'F' => loop {
                stream.write_all(&PATTERN).await?;
            },
            b'Q' => {
                let mut trailing = [0; 1];
                ensure!(
                    stream.read(&mut trailing).await? == 0,
                    "expected TLS close_notify"
                );
                stream.shutdown().await?;
                return Ok(());
            }
            _ => bail!("invalid benchmark command"),
        }
    }
}

fn proxy(source: SocketAddr, destination: SocketAddr) -> Vec<u8> {
    let mut b = b"\r\n\r\n\0\r\nQUIT\n\x21".to_vec();
    match (source, destination) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => {
            b.push(0x11);
            b.extend_from_slice(&12u16.to_be_bytes());
            b.extend_from_slice(&s.ip().octets());
            b.extend_from_slice(&d.ip().octets());
        }
        (SocketAddr::V6(s), SocketAddr::V6(d)) => {
            b.push(0x21);
            b.extend_from_slice(&36u16.to_be_bytes());
            b.extend_from_slice(&s.ip().octets());
            b.extend_from_slice(&d.ip().octets());
        }
        _ => unreachable!("one public TCP socket has one address family"),
    }
    b.extend_from_slice(&source.port().to_be_bytes());
    b.extend_from_slice(&destination.port().to_be_bytes());
    b
}
