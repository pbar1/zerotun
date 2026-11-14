//! Fixed-target TCP/UDP forwarding over a mutually authenticated QUIC backhaul.

mod config;
mod forward;
mod metadata;
mod quic;
mod sockets;
#[cfg(test)]
mod tests;
mod udp;

use std::future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use mimalloc::MiMalloc;
use quinn::Connection;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::signal;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time;

use crate::config::Cli;
use crate::config::Command;
use crate::config::Edge;
use crate::config::Origin;
use crate::metadata::Metadata;
use crate::quic::Role;
use crate::quic::Stream;

#[global_allocator]
static ALLOCATOR: MiMalloc = MiMalloc;

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Edge(c) => edge(Arc::new(c), shutdown()).await,
        Command::Origin(c) => origin(Arc::new(c), shutdown()).await,
    }
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix;
        use tokio::signal::unix::SignalKind;

        let mut terminate = unix::signal(SignalKind::terminate())?;
        tokio::select! {
            r = signal::ctrl_c() => r?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    signal::ctrl_c().await?;
    Ok(())
}

async fn edge(c: Arc<Edge>, stop: impl Future<Output = Result<()>>) -> Result<()> {
    let listener = if let Some(addr) = c.listen_tcp {
        let listener = sockets::tcp_listener(addr).context("bind public TCP listener")?;
        eprintln!("TCP listening {}", listener.local_addr()?);
        Some(listener)
    } else {
        None
    };
    let socket = if let Some(addr) = c.listen_udp {
        let socket =
            UdpSocket::from_std(sockets::udp_socket(addr)?).context("bind public UDP listener")?;
        eprintln!("UDP listening {}", socket.local_addr()?);
        Some(socket)
    } else {
        None
    };
    let endpoint = quic::endpoint(&c.transport, c.bind, Role::Edge, socket.is_some())?;
    let (state, connection) = watch::channel(None);
    let tcp_connection = connection.clone();
    let result = tokio::select! {
        r = quic::accept(&endpoint, &c.transport, state) => r,
        r = async {
            if let Some(listener) = listener {
                edge_tcp(listener, c.clone(), tcp_connection).await
            } else { future::pending().await }
        } => r,
        r = async {
            if let Some(socket) = socket {
                udp::edge(socket, c.transport.clone(), connection).await
            } else { future::pending().await }
        } => r,
        r = stop => r,
    };
    endpoint.close(0u32.into(), b"shutdown");
    let _ = time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    result
}

async fn edge_tcp(
    listener: TcpListener,
    c: Arc<Edge>,
    connection: watch::Receiver<Option<Connection>>,
) -> Result<()> {
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let (tcp, source) = incoming?;
                if workers.len() >= c.transport.max_connections as usize { continue; }
                let Some(conn) = connection.borrow().clone() else { continue; };
                let source = sockets::canonical(source);
                let destination = sockets::canonical(tcp.local_addr()?);
                tcp.set_nodelay(true)?;
                let c = c.clone();
                workers.spawn(async move {
                    let result: Result<()> = async {
                        let (send, recv) =
                            time::timeout(c.transport.connect_timeout, conn.open_bi()).await??;
                        let mut stream = Stream { send, recv };
                        let meta = Metadata { source, destination };
                        time::timeout(
                            c.transport.connect_timeout,
                            stream.write_all(&meta.encode()?),
                        ).await??;
                        forward::relay(tcp, stream, c.transport.idle_timeout).await
                    }.await;
                    if let Err(error) = result { eprintln!("TCP {source}: {error:#}"); }
                });
            },
            Some(result) = workers.join_next(), if !workers.is_empty() => { result?; },
        }
    }
}

async fn origin(c: Arc<Origin>, stop: impl Future<Output = Result<()>>) -> Result<()> {
    let bind = if c.edge.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        ([0u16; 8], 0).into()
    };
    let endpoint = quic::endpoint(&c.transport, bind, Role::Origin, c.target_udp.is_some())?;
    let (tx, rx) = mpsc::channel(c.transport.max_connections as usize);
    let result = tokio::select! {
        r = quic::connect(&endpoint, c.clone(), tx) => r,
        r = origin_tcp(c, rx) => r,
        r = stop => r,
    };
    endpoint.close(0u32.into(), b"shutdown");
    let _ = time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    result
}

async fn origin_tcp(c: Arc<Origin>, mut rx: mpsc::Receiver<Stream>) -> Result<()> {
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            incoming = rx.recv() => {
                let Some(mut stream) = incoming else { bail!("backhaul driver stopped"); };
                let Some(target) = c.target_tcp else { continue; };
                if workers.len() >= c.transport.max_connections as usize { continue; }
                let c = c.clone();
                workers.spawn(async move {
                    let result: Result<()> = async {
                        let mut b = [0; metadata::SIZE];
                        time::timeout(c.transport.connect_timeout, stream.read_exact(&mut b))
                            .await??;
                        let meta = Metadata::decode(&b)?;
                        let mut tcp = time::timeout(
                            c.transport.connect_timeout,
                            TcpStream::connect(target),
                        ).await??;
                        tcp.set_nodelay(true)?;
                        if c.proxy_tcp {
                            time::timeout(
                                c.transport.connect_timeout,
                                tcp.write_all(&meta.proxy_v2()?),
                            ).await??;
                        }
                        forward::relay(tcp, stream, c.transport.idle_timeout).await
                    }.await;
                    if let Err(error) = result {
                        eprintln!("TCP target {target}: {error:#}");
                    }
                });
            },
            Some(result) = workers.join_next(), if !workers.is_empty() => { result?; },
        }
    }
}
