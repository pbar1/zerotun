//! Opaque public UDP forwarding through native QUIC DATAGRAM frames.
//! Fragmentation is bounded and unreliable: incomplete packets expire, never
//! retransmit.

use std::collections::HashMap;
use std::future;
use std::io;
use std::io::ErrorKind;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::ensure;
use bytes::Bytes;
use nix::cmsg_space;
use nix::libc::in_pktinfo;
use nix::libc::in6_pktinfo;
use nix::sys::socket::ControlMessage;
use nix::sys::socket::ControlMessageOwned;
use nix::sys::socket::MsgFlags;
use nix::sys::socket::SockaddrStorage;
use nix::sys::socket::recvmsg;
use nix::sys::socket::sendmsg;
use nix::sys::socket::setsockopt;
use nix::sys::socket::sockopt::Ipv4PacketInfo;
use nix::sys::socket::sockopt::Ipv6RecvPacketInfo;
use quinn::Connection;
use tokio::io::Interest;
use tokio::net::UdpSocket;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time;
use tokio::time::Instant;
use tokio::time::sleep_until;

use crate::config::Origin;
use crate::config::Transport;
use crate::metadata::Metadata;
use crate::metadata::SIZE;

pub const MAX_PACKET: usize = 65507;
const HEADER: usize = 18 + SIZE;
const MAX_FRAGMENTS: usize = 64;
const MAX_PENDING: usize = 64;
const REASSEMBLY_BYTES: usize = 4 * 1024 * 1024;
const QUEUE_PACKETS: usize = 64;
const QUEUE_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy)]
enum PacketInfo {
    V4(in_pktinfo),
    V6(in6_pktinfo),
}
impl PacketInfo {
    fn destination(self) -> IpAddr {
        match self {
            Self::V4(info) => Ipv4Addr::from(info.ipi_addr.s_addr.to_ne_bytes()).into(),
            Self::V6(info) => Ipv6Addr::from(info.ipi6_addr.s6_addr).into(),
        }
    }
}

#[derive(Clone, Copy)]
struct Reply {
    peer: SocketAddr,
    info: PacketInfo,
}

async fn receive(
    socket: &UdpSocket,
    local: SocketAddr,
    buffer: &mut [u8],
    control: &mut [u8],
) -> io::Result<Option<(usize, Metadata, Reply)>> {
    socket
        .async_io(Interest::READABLE, || {
            let mut buffers = [IoSliceMut::new(buffer)];
            let packet = recvmsg::<SockaddrStorage>(
                socket.as_raw_fd(),
                &mut buffers,
                Some(control),
                MsgFlags::empty(),
            )?;
            if packet
                .flags
                .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
            {
                return Ok(None);
            }
            let Some(address) = packet.address else {
                return Ok(None);
            };
            let peer = if let Some(v4) = address.as_sockaddr_in() {
                SocketAddr::from(*v4)
            } else if let Some(v6) = address.as_sockaddr_in6() {
                SocketAddr::from(*v6)
            } else {
                return Ok(None);
            };
            let mut info = None;
            for message in packet.cmsgs()? {
                match message {
                    ControlMessageOwned::Ipv4PacketInfo(value) => {
                        info = Some(PacketInfo::V4(value));
                    }
                    ControlMessageOwned::Ipv6PacketInfo(value) => {
                        info = Some(PacketInfo::V6(value));
                    }
                    _ => {}
                }
            }
            let Some(info) = info else {
                return Ok(None);
            };
            let meta = Metadata {
                source: SocketAddr::new(peer.ip().to_canonical(), peer.port()),
                destination: SocketAddr::new(info.destination().to_canonical(), local.port()),
            };
            Ok(Some((packet.bytes, meta, Reply { peer, info })))
        })
        .await
}

async fn reply(socket: &UdpSocket, route: Reply, body: &[u8]) -> io::Result<()> {
    let destination = SockaddrStorage::from(route.peer);
    let mut info = route.info;
    if let PacketInfo::V4(value) = &mut info {
        value.ipi_spec_dst = value.ipi_addr;
    }
    socket
        .async_io(Interest::WRITABLE, || {
            let message = match &info {
                PacketInfo::V4(value) => ControlMessage::Ipv4PacketInfo(value),
                PacketInfo::V6(value) => ControlMessage::Ipv6PacketInfo(value),
            };
            sendmsg(
                socket.as_raw_fd(),
                &[IoSlice::new(body)],
                &[message],
                MsgFlags::empty(),
                Some(&destination),
            )
            .map(|_| ())
            .map_err(io::Error::from)
        })
        .await
}

struct Request {
    body: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}
struct Flow {
    tx: mpsc::Sender<Request>,
    budget: Arc<Semaphore>,
}

struct Header {
    meta: Metadata,
    id: u64,
    total: usize,
    offset: usize,
    chunk: usize,
    count: usize,
}
fn header(b: &[u8]) -> Result<Header> {
    ensure!(b.len() >= HEADER && &b[..4] == b"RUD1", "UDP frame version");
    let id = u64::from_be_bytes(b[4..12].try_into()?);
    let total = u16::from_be_bytes(b[12..14].try_into()?) as usize;
    let offset = u16::from_be_bytes(b[14..16].try_into()?) as usize;
    let chunk = u16::from_be_bytes(b[16..18].try_into()?) as usize;
    ensure!(
        id != 0 && total <= MAX_PACKET && chunk != 0 && chunk <= MAX_PACKET,
        "UDP bounds"
    );
    let count = total.div_ceil(chunk).max(1);
    ensure!(
        count <= MAX_FRAGMENTS && offset <= total && offset.is_multiple_of(chunk),
        "UDP fragment offset"
    );
    ensure!(
        offset / chunk < count && b.len() - HEADER == chunk.min(total - offset),
        "UDP fragment length"
    );
    let meta = Metadata::decode(b[18..HEADER].try_into()?)?;
    Ok(Header {
        meta,
        id,
        total,
        offset,
        chunk,
        count,
    })
}

fn frames(meta: Metadata, id: u64, packet: &[u8], max_datagram: usize) -> Result<Vec<Bytes>> {
    ensure!(
        packet.len() <= MAX_PACKET && id != 0,
        "UDP packet too large"
    );
    let chunk = max_datagram
        .checked_sub(HEADER)
        .filter(|n| *n != 0)
        .ok_or_else(|| anyhow::anyhow!("QUIC datagram too small"))?
        .min(MAX_PACKET);
    let count = packet.len().div_ceil(chunk).max(1);
    ensure!(count <= MAX_FRAGMENTS, "too many UDP fragments");
    let metadata = meta.encode()?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let offset = i * chunk;
        let part = &packet[offset..(offset + chunk).min(packet.len())];
        let mut b = Vec::with_capacity(HEADER + part.len());
        b.extend_from_slice(b"RUD1");
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&u16::try_from(packet.len())?.to_be_bytes());
        b.extend_from_slice(&u16::try_from(offset)?.to_be_bytes());
        b.extend_from_slice(&u16::try_from(chunk)?.to_be_bytes());
        b.extend_from_slice(&metadata);
        b.extend_from_slice(part);
        out.push(Bytes::from(b));
    }
    Ok(out)
}

fn send(conn: &Connection, meta: Metadata, id: u64, packet: &[u8]) -> Result<bool> {
    let max = conn
        .max_datagram_size()
        .ok_or_else(|| anyhow::anyhow!("peer has no QUIC DATAGRAM support"))?;
    let count = packet
        .len()
        .div_ceil(max.saturating_sub(HEADER).max(1))
        .max(1);
    if conn.datagram_send_buffer_space() < packet.len() + count * HEADER {
        return Ok(false);
    }
    for frame in frames(meta, id, packet, max)? {
        conn.send_datagram(frame)?;
    }
    Ok(true)
}

struct Pending {
    body: Vec<u8>,
    chunk: usize,
    seen: u64,
    count: usize,
    created: Instant,
}
struct Assembler {
    packets: HashMap<(Metadata, u64), Pending>,
    reserved: usize,
    ttl: Duration,
}
impl Assembler {
    fn new(idle: Duration) -> Self {
        Self {
            packets: HashMap::new(),
            reserved: 0,
            ttl: idle.min(Duration::from_secs(2)),
        }
    }
    fn clear(&mut self) {
        self.packets.clear();
        self.reserved = 0;
    }
    fn expire(&mut self, now: Instant) {
        self.packets.retain(|_, p| {
            if now.duration_since(p.created) >= self.ttl {
                self.reserved -= p.body.len();
                false
            } else {
                true
            }
        });
    }
    fn push(&mut self, b: &[u8], now: Instant) -> Result<Option<(Metadata, Vec<u8>)>> {
        let h = header(b)?;
        if h.count == 1 {
            return Ok(Some((h.meta, b[HEADER..].to_vec())));
        }
        let key = (h.meta, h.id);
        if !self.packets.contains_key(&key) {
            ensure!(
                self.packets.len() < MAX_PENDING && self.reserved + h.total <= REASSEMBLY_BYTES,
                "UDP reassembly full"
            );
            self.reserved += h.total;
            self.packets.insert(
                key,
                Pending {
                    body: vec![0; h.total],
                    chunk: h.chunk,
                    seen: 0,
                    count: h.count,
                    created: now,
                },
            );
        }
        let p = self
            .packets
            .get_mut(&key)
            .expect("fragment entry was inserted");
        ensure!(
            p.body.len() == h.total && p.chunk == h.chunk && p.count == h.count,
            "inconsistent UDP fragments"
        );
        let bit = 1u64 << (h.offset / h.chunk);
        if p.seen & bit != 0 {
            return Ok(None);
        }
        p.body[h.offset..h.offset + b.len() - HEADER].copy_from_slice(&b[HEADER..]);
        p.seen |= bit;
        if p.seen.count_ones() as usize != p.count {
            return Ok(None);
        }
        let p = self
            .packets
            .remove(&key)
            .expect("completed fragment entry exists");
        self.reserved -= p.body.len();
        Ok(Some((h.meta, p.body)))
    }
}

pub async fn edge(
    socket: UdpSocket,
    c: Transport,
    mut state: watch::Receiver<Option<Connection>>,
) -> Result<()> {
    let local = socket.local_addr()?;
    if local.is_ipv4() {
        setsockopt(&socket, Ipv4PacketInfo, &true)?;
    } else {
        setsockopt(&socket, Ipv6RecvPacketInfo, &true)?;
    }
    let mut control = cmsg_space!(in_pktinfo, in6_pktinfo);
    let mut conn = state.borrow().clone();
    let mut flows: HashMap<Metadata, (Reply, Instant)> = HashMap::new();
    let mut assembly = Assembler::new(c.idle_timeout);
    let mut b = vec![0; MAX_PACKET + 1];
    let mut sequence = 0u64;
    let mut tick = time::interval((c.idle_timeout / 2).min(Duration::from_millis(250)));
    loop {
        let receiver = conn.clone();
        tokio::select! {
            changed = state.changed() => {
                changed?; conn.clone_from(&state.borrow()); flows.clear(); assembly.clear();
            },
            incoming = receive(&socket, local, &mut b, &mut control) => {
                let (n, meta, route) = match incoming {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(e) if matches!(
                        e.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
                            | ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable
                    ) => { continue; },
                    Err(e) => return Err(e.into()),
                };
                let Some(active) = &conn else { continue; };
                if n > MAX_PACKET || active.close_reason().is_some() { continue; }
                if !flows.contains_key(&meta) && flows.len() >= c.max_connections as usize {
                    continue;
                }
                flows.insert(meta, (route, Instant::now()));
                sequence = sequence.checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("UDP sequence exhausted"))?;
                let _ = send(active, meta, sequence, &b[..n]);
            },
            packet = async {
                if let Some(active) = receiver {
                    active.read_datagram().await
                } else { future::pending().await }
            } => {
                let Ok(packet) = packet else {
                    conn = None; flows.clear(); assembly.clear(); continue;
                };
                let Ok(h) = header(&packet) else { continue; };
                // Replies can only target an existing socket-derived client association.
                if !flows.contains_key(&h.meta) { continue; }
                if let Ok(Some((meta, body))) = assembly.push(&packet, Instant::now()) {
                    let route = flows[&meta].0;
                    if reply(&socket, route, &body).await.is_ok() {
                        flows.insert(meta, (route, Instant::now()));
                    }
                }
            },
            _ = tick.tick() => {
                let now = Instant::now(); assembly.expire(now);
                flows.retain(|_, (_, seen)| now.duration_since(*seen) < c.idle_timeout);
            }
        }
    }
}

async fn association(
    meta: Metadata,
    socket: UdpSocket,
    conn: Connection,
    c: Arc<Origin>,
    sequence: Arc<AtomicU64>,
    mut incoming: mpsc::Receiver<Request>,
) -> Result<()> {
    let mut b = vec![0; MAX_PACKET + 1];
    let mut deadline = Instant::now() + c.transport.idle_timeout;
    loop {
        tokio::select! {
            packet = incoming.recv() => {
                let Some(packet) = packet else { break; };
                if c.proxy_udp {
                    let mut header = meta.proxy_v2_datagram()?;
                    if header.len() + packet.body.len() > MAX_PACKET { continue; }
                    header.extend_from_slice(&packet.body); socket.send(&header).await?;
                } else { socket.send(&packet.body).await?; }
                deadline = Instant::now() + c.transport.idle_timeout;
            },
            n = socket.recv(&mut b) => {
                let n = n?; if n > MAX_PACKET { continue; }
                let id = sequence
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
                    .map_err(|_| anyhow::anyhow!("UDP sequence exhausted"))?;
                let _ = send(&conn, meta, id, &b[..n]);
                deadline = Instant::now() + c.transport.idle_timeout;
            },
            () = sleep_until(deadline) => break,
            _ = conn.closed() => break,
        }
    }
    Ok(())
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "Flows remain in use across select! arms and loop iterations."
)]
pub async fn origin(conn: Connection, c: Arc<Origin>) -> Result<()> {
    let target = c
        .target_udp
        .ok_or_else(|| anyhow::anyhow!("missing UDP destination"))?;
    let mut flows: HashMap<Metadata, Flow> = HashMap::new();
    let mut workers = JoinSet::new();
    let sequence = Arc::new(AtomicU64::new(1));
    let mut assembly = Assembler::new(c.transport.idle_timeout);
    let mut tick = time::interval((c.transport.idle_timeout / 2).min(Duration::from_millis(250)));
    loop {
        tokio::select! {
            packet = conn.read_datagram() => {
                let packet = packet?;
                let Ok(complete) = assembly.push(&packet, Instant::now()) else { continue; };
                let Some((meta, body)) = complete else { continue; };
                if !flows.contains_key(&meta) {
                    if flows.len() >= c.transport.max_connections as usize
                        || workers.len() >= c.transport.max_connections as usize {
                        continue;
                    }
                    let bind: SocketAddr = if target.is_ipv4() {
                        "0.0.0.0:0".parse()?
                    } else { "[::]:0".parse()? };
                    let Ok(socket) = UdpSocket::bind(bind).await else { continue; };
                    if socket.connect(target).await.is_err() { continue; }
                    let (tx, rx) = mpsc::channel(QUEUE_PACKETS);
                    flows.insert(meta, Flow { tx, budget: Arc::new(Semaphore::new(QUEUE_BYTES)) });
                    let (conn, c, sequence) = (conn.clone(), c.clone(), sequence.clone());
                    workers.spawn(async move {
                        (meta, association(meta, socket, conn, c, sequence, rx).await)
                    });
                }
                if let Some(flow) = flows.get(&meta) {
                    // Absorb small-packet bursts without allowing 64 maximum-size packets.
                    if let Ok(permit) = flow.budget.clone()
                        .try_acquire_many_owned(u32::try_from(body.len().max(1))?) {
                        let _ = flow.tx.try_send(Request { body, _budget: permit });
                    }
                }
            },
            result = workers.join_next(), if !workers.is_empty() => {
                if let Some(Ok((meta, _))) = result { flows.remove(&meta); }
            },
            _ = tick.tick() => {
                assembly.expire(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn meta() -> Metadata {
        Metadata {
            source: "127.0.0.1:1234".parse().expect("valid fixture address"),
            destination: "127.0.0.1:443".parse().expect("valid fixture address"),
        }
    }
    #[test]
    fn boundaries_reordering_duplicates_and_expiry() -> Result<()> {
        for n in [0, 1, 1200, 1350, MAX_PACKET] {
            for size in [1162, 1414] {
                let body = vec![91; n];
                let encoded = frames(meta(), 1, &body, size)?;
                assert!(encoded.iter().all(|b| b.len() <= size));
                let mut receiver = Assembler::new(Duration::from_secs(1));
                let mut complete = None;
                for frame in encoded.iter().rev() {
                    if let Some(p) = receiver.push(frame, Instant::now())? {
                        complete = Some(p);
                    }
                }
                assert_eq!(complete.expect("all fragments reassembled").1, body);
                assert_eq!(receiver.reserved, 0);
            }
        }
        let encoded = frames(meta(), 2, &vec![0; MAX_PACKET], 1162)?;
        let now = Instant::now();
        let mut receiver = Assembler::new(Duration::from_secs(1));
        assert!(receiver.push(&encoded[0], now)?.is_none());
        assert!(receiver.push(&encoded[0], now)?.is_none());
        assert_eq!(receiver.reserved, MAX_PACKET);
        receiver.expire(now + Duration::from_secs(2));
        assert_eq!(receiver.reserved, 0);
        let mut invalid = encoded[0].to_vec();
        invalid[16..18].fill(0);
        assert!(receiver.push(&invalid, now).is_err());
        for id in 1..=MAX_PENDING as u64 {
            receiver.push(&frames(meta(), id, &vec![0; MAX_PACKET], 1162)?[0], now)?;
        }
        assert!(
            receiver
                .push(&frames(meta(), 999, &vec![0; MAX_PACKET], 1162)?[0], now)
                .is_err()
        );
        assert!(receiver.reserved <= REASSEMBLY_BYTES);
        Ok(())
    }
}
