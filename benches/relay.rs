//! End-to-end relay measurements with verified payloads and a same-host regression gate.

#![allow(
    clippy::large_stack_arrays,
    clippy::large_futures,
    reason = "Benchmark fixtures reuse 64 KiB buffers without adding allocations to measurements."
)]

mod support {
    pub mod credentials;
    pub mod fixture;
    pub mod link;
    pub mod regression;
}

use std::env;
use std::mem;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::SamplingMode;
use criterion::Throughput;
use futures_util::future;
use tokio::runtime::Builder;
use tokio::runtime::Runtime;
use tokio::task::JoinSet;
use tokio::time;

use crate::support::fixture::CHUNK;
use crate::support::fixture::Client;
use crate::support::fixture::DEADLINE;
use crate::support::fixture::Fixture;
use crate::support::fixture::Network;
use crate::support::regression;

const UDP_LARGE: usize = if cfg!(target_os = "macos") {
    8192
} else {
    32768
};

async fn transfer(
    clients: &mut Vec<Client>,
    iterations: u64,
    bytes: usize,
    duplex: bool,
) -> Duration {
    let start = Instant::now();
    let mut workers = JoinSet::new();
    for mut client in mem::take(clients) {
        workers.spawn(async move {
            for _ in 0..iterations {
                time::timeout(DEADLINE, async {
                    if duplex {
                        client.duplex(bytes).await
                    } else {
                        client.download(bytes).await
                    }
                })
                .await??;
            }
            Ok::<_, anyhow::Error>(client)
        });
    }
    while let Some(result) = workers.join_next().await {
        clients.push(
            result
                .expect("bulk transfer task panicked")
                .expect("bulk transfer failed"),
        );
    }
    start.elapsed()
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "Benchmark groups remain borrowed throughout the measurements until finish."
)]
fn controller(c: &mut Criterion, runtime: &Runtime, congestion: &str) -> Result<()> {
    let mut fixture = runtime.block_on(Fixture::start(congestion, Network::Loopback))?;
    let mut group = c.benchmark_group(format!("loopback/{congestion}"));
    group.sampling_mode(SamplingMode::Flat);
    group.bench_function("connect_and_first_response", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let start = Instant::now();
                    let mut client = fixture.client().await.expect("connect application mTLS");
                    time::timeout(DEADLINE, client.echo(128))
                        .await
                        .expect("echo exceeded deadline")
                        .expect("echo failed");
                    elapsed += start.elapsed();
                    client.close().await.expect("close measured connection");
                }
                elapsed
            })
        });
        fixture
            .check()
            .expect("benchmark tunnel must remain healthy");
    });
    let mut client = runtime.block_on(fixture.client())?;
    group.bench_function("interactive_128b", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let start = Instant::now();
                for _ in 0..iterations {
                    time::timeout(DEADLINE, client.echo(128))
                        .await
                        .expect("echo exceeded deadline")
                        .expect("echo failed");
                }
                start.elapsed()
            })
        });
        fixture
            .check()
            .expect("benchmark tunnel must remain healthy");
    });
    runtime.block_on(client.close())?;
    for connections in [1, 2, 10, 100] {
        // Keep a similar aggregate payload while amortizing connection setup.
        let per_stream = (16 * 1024 * 1024 / connections / CHUNK).max(1) * CHUNK;
        let mut clients = runtime.block_on(fixture.clients(connections))?;
        group.throughput(Throughput::Bytes((per_stream * connections) as u64));
        group.bench_with_input(
            BenchmarkId::new("download", connections),
            &per_stream,
            |b, &bytes| {
                b.iter_custom(|iterations| {
                    runtime
                        .block_on(async { transfer(&mut clients, iterations, bytes, false).await })
                });
                fixture
                    .check()
                    .expect("benchmark tunnel must remain healthy");
            },
        );
        runtime.block_on(future::try_join_all(clients.into_iter().map(Client::close)))?;
    }
    let mut clients = runtime.block_on(fixture.clients(2))?;
    let bytes = 4 * 1024 * 1024;
    group.throughput(Throughput::Bytes((bytes * 2 * 2) as u64));
    group.bench_function("duplex_2_streams", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async { transfer(&mut clients, iterations, bytes, true).await })
        });
        fixture
            .check()
            .expect("benchmark tunnel must remain healthy");
    });
    runtime.block_on(future::try_join_all(clients.into_iter().map(Client::close)))?;
    for size in [1200, UDP_LARGE] {
        let mut client = runtime.block_on(fixture.udp())?;
        // Warm up the association outside the measurement.
        runtime.block_on(client.exchange(size))?;
        group.throughput(Throughput::Bytes((size * 2) as u64));
        group.bench_with_input(BenchmarkId::new("udp_echo", size), &size, |b, &size| {
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let start = Instant::now();
                    for _ in 0..iterations {
                        client.exchange(size).await.expect("UDP echo failed");
                    }
                    start.elapsed()
                })
            });
            fixture
                .check()
                .expect("benchmark tunnel must remain healthy");
        });
    }
    group.finish();
    drop(fixture);

    let mut fixture = runtime.block_on(Fixture::start(congestion, Network::Ipv6))?;
    c.bench_function(
        &format!("ipv6/{congestion}/connect_and_first_response"),
        |b| {
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let start = Instant::now();
                        let mut client = fixture.client().await.expect("connect application mTLS");
                        time::timeout(DEADLINE, client.echo(128))
                            .await
                            .expect("echo exceeded deadline")
                            .expect("echo failed");
                        elapsed += start.elapsed();
                        client.close().await.expect("close measured connection");
                    }
                    elapsed
                })
            });
            fixture
                .check()
                .expect("benchmark tunnel must remain healthy");
        },
    );
    drop(fixture);

    let mut fixture = runtime.block_on(Fixture::start(congestion, Network::Metro))?;
    let mut group = c.benchmark_group(format!("metro/{congestion}"));
    group.sampling_mode(SamplingMode::Flat);
    let mut clients = runtime.block_on(fixture.clients(2))?;
    let bytes = 8 * 1024 * 1024;
    group.throughput(Throughput::Bytes((bytes * 2) as u64));
    group.bench_function("download_2_streams", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async { transfer(&mut clients, iterations, bytes, false).await })
        });
        fixture
            .check()
            .expect("benchmark tunnel must remain healthy");
    });
    runtime.block_on(future::try_join_all(clients.into_iter().map(Client::close)))?;
    // A new group avoids carrying throughput labels onto latency measurements.
    group.finish();
    let load = runtime.block_on(fixture.load())?;
    let mut client = runtime.block_on(fixture.client())?;
    c.bench_function(&format!("metro/{congestion}/loaded_tail_8"), |b| {
        // Each iteration is the slowest of eight interactive requests while two
        // independent, verified downloads compete for the persistent backhaul.
        // Criterion calibrates wall time, but estimates this tail-sensitive value.
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let before = load.progress();
                let mut sum = Duration::ZERO;
                for _ in 0..iterations {
                    let mut slowest = Duration::ZERO;
                    for _ in 0..8 {
                        let start = Instant::now();
                        time::timeout(DEADLINE, client.echo(128))
                            .await
                            .expect("echo exceeded deadline")
                            .expect("echo failed");
                        slowest = slowest.max(start.elapsed());
                    }
                    sum += slowest;
                    load.check();
                }
                assert!(
                    load.progress().into_iter().zip(before).all(|(a, b)| a > b),
                    "both background downloads must progress during measurement"
                );
                sum
            })
        });
        fixture
            .check()
            .expect("benchmark tunnel must remain healthy");
    });
    runtime.block_on(client.close())?;
    drop(load);
    Ok(())
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "Criterion is borrowed throughout controller runs before final_summary."
)]
fn main() -> Result<()> {
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let output = env::var_os("ZEROTUN_BENCH_OUTPUT").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/criterion"),
        PathBuf::from,
    );
    let mut criterion = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .nresamples(10000)
        .confidence_level(0.99)
        .significance_level(0.01)
        .without_plots()
        .output_directory(&output)
        .configure_from_args();
    for congestion in ["cubic", "bbr"] {
        controller(&mut criterion, &runtime, congestion)?;
    }
    criterion.final_summary();
    if let Ok(baseline) = env::var("ZEROTUN_BENCH_COMPARE") {
        let threshold = env::var("ZEROTUN_BENCH_MAX_SLOWDOWN")
            .unwrap_or_else(|_| "0.15".into())
            .parse()?;
        regression::check(&output, &baseline, threshold)?;
    }
    Ok(())
}
