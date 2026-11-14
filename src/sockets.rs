//! Socket binding with explicit IPv4/IPv6 behavior.

use std::io;
use std::net::SocketAddr;
use std::net::UdpSocket;

use socket2::Domain;
use socket2::Socket;
use socket2::Type;
use tokio::net::TcpListener;

fn bind(address: SocketAddr, kind: Type) -> io::Result<Socket> {
    let address = canonical(address);
    let socket = Socket::new(Domain::for_address(address), kind, None)?;
    if kind == Type::STREAM {
        socket.set_reuse_address(true)?;
    }
    if address.is_ipv6() {
        socket.set_only_v6(!address.ip().is_unspecified())?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    Ok(socket)
}

pub fn tcp_listener(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = bind(address, Type::STREAM)?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

pub fn udp_socket(address: SocketAddr) -> io::Result<UdpSocket> {
    Ok(bind(address, Type::DGRAM)?.into())
}

pub fn canonical(address: SocketAddr) -> SocketAddr {
    if let SocketAddr::V6(v6) = address
        && let Some(ip) = v6.ip().to_ipv4_mapped()
    {
        return SocketAddr::new(ip.into(), v6.port());
    }
    address
}
