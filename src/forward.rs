use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use tokio::io as tokio_io;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::time;
use tokio::time::Instant;

pub const BUFFER: usize = 16 * 1024;

struct Activity<T> {
    io: T,
    start: Instant,
    last: Arc<AtomicU64>,
}
impl<T> Activity<T> {
    fn touch(&self) {
        self.last.store(
            self.start
                .elapsed()
                .as_millis()
                .try_into()
                .expect("uptime fits in u64 milliseconds"),
            Ordering::Relaxed,
        );
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for Activity<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let n = b.filled().len();
        let r = Pin::new(&mut self.io).poll_read(cx, b);
        if b.filled().len() > n {
            self.touch();
        }
        r
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Activity<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.io).poll_write(cx, b);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.touch();
        }
        r
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

pub async fn relay<A, B>(a: A, b: B, idle: Duration) -> Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let mut a = Activity {
        io: a,
        start,
        last: last.clone(),
    };
    let mut b = Activity {
        io: b,
        start,
        last: last.clone(),
    };
    let timeout = async {
        loop {
            time::sleep(idle.min(Duration::from_secs(1))).await;
            if start
                .elapsed()
                .saturating_sub(Duration::from_millis(last.load(Ordering::Relaxed)))
                >= idle
            {
                break;
            }
        }
    };
    tokio::select! {
        r = tokio_io::copy_bidirectional_with_sizes(&mut a, &mut b, BUFFER, BUFFER) => {
            r?;
            Ok(())
        },
        () = timeout => bail!("idle timeout"),
    }
}
