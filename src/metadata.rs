use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;

pub const SIZE: usize = 44;
pub const PROXY_MAGIC: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Metadata {
    pub source: SocketAddr,
    pub destination: SocketAddr,
}

impl Metadata {
    pub fn encode(self) -> Result<[u8; SIZE]> {
        let mut b = [0; SIZE];
        b[..4].copy_from_slice(b"RLY\0");
        b[4] = 1;
        match (self.source.ip(), self.destination.ip()) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                b[5] = 4;
                b[8..12].copy_from_slice(&s.octets());
                b[24..28].copy_from_slice(&d.octets());
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                b[5] = 6;
                b[8..24].copy_from_slice(&s.octets());
                b[24..40].copy_from_slice(&d.octets());
            }
            _ => bail!("mixed address families"),
        }
        b[40..42].copy_from_slice(&self.source.port().to_be_bytes());
        b[42..44].copy_from_slice(&self.destination.port().to_be_bytes());
        Ok(b)
    }

    pub fn decode(b: &[u8; SIZE]) -> Result<Self> {
        ensure!(&b[..4] == b"RLY\0" && b[4] == 1, "metadata version");
        ensure!(b[6..8] == [0, 0], "metadata reserved fields");
        let (s, d) = match b[5] {
            4 => {
                ensure!(b[12..24] == [0; 12] && b[28..40] == [0; 12], "IPv4 padding");
                (
                    IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&b[8..12])?)),
                    IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&b[24..28])?)),
                )
            }
            6 => (
                IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&b[8..24])?)),
                IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&b[24..40])?)),
            ),
            _ => bail!("metadata family"),
        };
        Ok(Self {
            source: SocketAddr::new(s, u16::from_be_bytes(b[40..42].try_into()?)),
            destination: SocketAddr::new(d, u16::from_be_bytes(b[42..44].try_into()?)),
        })
    }

    pub fn proxy_v2(self) -> Result<Vec<u8>> {
        let mut b = PROXY_MAGIC.to_vec();
        b.push(0x21); // v2, PROXY command
        match (self.source.ip(), self.destination.ip()) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                b.push(0x11); // INET, STREAM
                b.extend_from_slice(&12u16.to_be_bytes());
                b.extend_from_slice(&s.octets());
                b.extend_from_slice(&d.octets());
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                b.push(0x21); // INET6, STREAM
                b.extend_from_slice(&36u16.to_be_bytes());
                b.extend_from_slice(&s.octets());
                b.extend_from_slice(&d.octets());
            }
            _ => bail!("mixed address families"),
        }
        b.extend_from_slice(&self.source.port().to_be_bytes());
        b.extend_from_slice(&self.destination.port().to_be_bytes());
        Ok(b)
    }

    pub fn proxy_v2_datagram(self) -> Result<Vec<u8>> {
        let mut b = self.proxy_v2()?;
        b[13] = (b[13] & 0xf0) | 0x02;
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_and_malformed() -> Result<()> {
        for (s, d, size) in [
            ("127.0.0.1:1234", "127.0.0.1:8888", 28),
            ("[::1]:1234", "[2001:db8::1]:8888", 52),
        ] {
            let m = Metadata {
                source: s.parse()?,
                destination: d.parse()?,
            };
            assert_eq!(Metadata::decode(&m.encode()?)?, m);
            assert_eq!(m.proxy_v2()?.len(), size);
            for offset in [0, 4, 5, 6, 7] {
                let mut bad = m.encode()?;
                bad[offset] = 255;
                assert!(Metadata::decode(&bad).is_err());
            }
        }
        let m = Metadata {
            source: "127.0.0.1:1".parse()?,
            destination: "127.0.0.1:2".parse()?,
        };
        let mut b = m.encode()?;
        b[15] = 1;
        assert!(Metadata::decode(&b).is_err());
        Ok(())
    }
}
