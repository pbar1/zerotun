use std::collections::VecDeque;
use std::net::SocketAddr;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time;
use tokio::time::Instant;

// A fixed regression fixture, not an Internet simulator: 15 ms RTT, 940 Mbit/s
// per direction, 1500-byte IPv4 MTU. No synthetic jitter or random loss.
pub struct Link {
    pub address: SocketAddr,
    dropped: Arc<AtomicU64>,
    tasks: Vec<JoinHandle<()>>,
}

fn socket() -> Result<UdpSocket> {
    let socket = StdUdpSocket::bind("127.0.0.1:0")?;
    let reference = socket2::SockRef::from(&socket);
    reference.set_recv_buffer_size(2 * 1024 * 1024)?;
    reference.set_send_buffer_size(2 * 1024 * 1024)?;
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket)?)
}

impl Link {
    pub fn start(edge: SocketAddr) -> Result<Self> {
        let front = Arc::new(socket()?);
        let back = Arc::new(socket()?);
        let address = front.local_addr()?;
        let (origin, learned) = watch::channel(None);
        let dropped = Arc::new(AtomicU64::new(0));
        let tasks = vec![
            tokio::spawn(direction(
                front.clone(),
                back.clone(),
                Some(edge),
                Some(origin),
                learned.clone(),
                dropped.clone(),
            )),
            tokio::spawn(direction(back, front, None, None, learned, dropped.clone())),
        ];
        Ok(Self {
            address,
            dropped,
            tasks,
        })
    }

    pub fn check(&self) {
        assert_eq!(
            self.dropped.load(Ordering::Relaxed),
            0,
            "link queue overflow or send failure invalidated the measurement"
        );
        assert!(
            self.tasks.iter().all(|t| !t.is_finished()),
            "link driver stopped"
        );
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn direction(
    input: Arc<UdpSocket>,
    output: Arc<UdpSocket>,
    fixed: Option<SocketAddr>,
    learn: Option<watch::Sender<Option<SocketAddr>>>,
    origin: watch::Receiver<Option<SocketAddr>>,
    dropped: Arc<AtomicU64>,
) {
    let mut queue = VecDeque::<(Instant, Vec<u8>, SocketAddr)>::new();
    let mut next = Instant::now();
    let mut buffer = [0u8; 65536];
    loop {
        let due = queue
            .front()
            .map_or_else(|| Instant::now() + Duration::from_hours(1), |p| p.0);
        tokio::select! {
            biased;
            () = time::sleep_until(due), if !queue.is_empty() => {
                // Bound work per wakeup, but release due packets in a batch rather
                // than rounding each packet's serialization to a 1 ms Tokio tick.
                for _ in 0..128 {
                    if queue.front().is_none_or(|p| p.0 > Instant::now()) { break; }
                    let (_, packet, destination) =
                        queue.pop_front().expect("queue front was just checked");
                    if output.send_to(&packet, destination).await.is_err() {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            packet = input.recv_from(&mut buffer) => {
                let Ok((n, source)) = packet else { return; };
                if let Some(learn) = &learn { learn.send_replace(Some(source)); }
                let destination = fixed.or_else(|| *origin.borrow());
                // Oversized PMTU probes are intentionally dropped. They are not
                // queue failures; the production endpoints retain PMTU discovery.
                if n + 28 > 1500 { continue; }
                let Some(destination) = destination else { continue; };
                if queue.len() >= 8192 {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                next = next.max(Instant::now())
                    + Duration::from_secs_f64(
                        f64::from(u16::try_from(n + 28).expect("within MTU")) * 8. / 940e6,
                    );
                queue.push_back((
                    next + Duration::from_micros(7500), buffer[..n].to_vec(), destination,
                ));
            },
        }
    }
}
