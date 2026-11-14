# Allocator comparison

Zerotun uses **mimalloc v3** for Rust heap allocations in both relay modes.
The main reason is lower retained RSS after connection churn, with similar
throughput, CPU cost and latency to the system allocator. This choice has a
tradeoff: mimalloc used more memory at the peak of the full stress workload.

The dependency is `mimalloc` 0.1.52 with default features disabled. The lockfile
selects `libmimalloc-sys` 0.1.49, which builds native mimalloc **3.3.2** by default.
The `v2`, `secure` and C-allocation `override` features are disabled. The allocator
is registered in the relay binary; benchmark clients and servers keep their own
allocator. No allocator configuration flags or runtime environment are required.

## Measurement scope

Measured on 2026-11-14 against relay source `3e7d642`, before the allocator change.
The five retained configurations completed **45 BBR performance sessions**:
three repetitions of metro, loopback and degraded-link sessions per allocator.
Another 15 sessions used separate counting binaries. Previously completed CUBIC
runs are a partial reference, with one or two repetitions; the remaining CUBIC
work was cancelled. Scudo, snmalloc and rpmalloc were screened out and are not
included in the final comparison below.

All variants used Rust 1.99.1, the GNU/Linux x86-64 target, glibc 2.44, the same
thin-LTO release profile and two Tokio workers per relay. The client, server,
link fixture and two relay processes shared four physical CPU cores under a
four-CPU / 4 GiB cgroup quota. These are local measurements with synthetic link
conditions. They do not remeasure the [historical WAN path](performance.md).

| Configuration | Tested implementation |
| --- | --- |
| Default | glibc 2.44, untuned `System` allocator |
| mimalloc v3 | `mimalloc` 0.1.52 / native 3.3.2 |
| mimalloc secure | Same native version, `MI_SECURE=4`, `MI_DEBUG=0` |
| jemalloc | `tikv-jemallocator` 0.7.0 / native 5.3.1 |
| tcmalloc | `tcmalloc-better` 0.1.19, default 8 KiB pages |

Every TCP connection verified its original source/destination through PROXY v2
and used application TLS 1.3 with client and server certificates. A separate CA
and exact peer pins authenticated the backhaul. The relays remained opaque to
application TLS. All received payloads were verified. UDP used opaque echo
datagrams with verified contents and boundaries, rather than application TLS.

The metro model used 15 ms RTT, 0.5 ms one-way jitter standard deviation,
0.005% random loss in each direction, 940 Mbit/s per direction and MTU 1500.
The degraded model used 35 ms RTT, 3 ms one-way jitter standard deviation,
0.2% loss per direction and 500 Mbit/s per direction. Both used seeded jitter,
FIFO delivery and bounded userspace queues over ordinary UDP sockets. Queue
overflow or a fixture send error invalidated a session. These model parameters
are estimates, not measurements of a particular Internet route.

## Paced streaming workload

Two 22.22 Mbit/s streams used steady delivery and one-second bursts for 20 seconds
each, alongside a 50-request/s, 128-byte interactive probe. The table describes
the burst workload.

Performance entries are three-run means ± sample standard deviation. Forward
p99 pools every observation from complete-population HDR histograms, with three
significant digits and microsecond resolution. It measures one-way application
delivery of the probe, including endpoint TLS and scheduling. CPU and RSS sum
edge and origin; CPU excludes the load generator, backend and link fixture.

| Allocator | Goodput, Mbit/s | CPU-seconds/GiB | Forward p99, ms | Sampled peak RSS, MiB | Allocation requests/GiB |
| --- | --- | --- | --- | --- | --- |
| Default | 44.11 ± 0.01 | 7.78 ± 0.15 | 10.455 | 21.4 ± 1.6 | 1,951,982 |
| mimalloc v3 | 44.11 ± 0.01 | 7.87 ± 0.15 | 10.415 | 23.4 ± 0.5 | 1,937,549 |
| mimalloc secure | 44.09 ± 0.03 | 8.07 ± 0.34 | 10.471 | 30.2 ± 1.2 | 1,922,489 |
| jemalloc | 44.09 ± 0.04 | 7.91 ± 0.17 | 10.391 | 29.7 ± 1.9 | 1,937,886 |
| tcmalloc | 44.08 ± 0.04 | 7.68 ± 0.11 | 10.463 | 39.1 ± 0.9 | 1,933,866 |

The offered workload limits goodput here; equal delivery does not establish
equal capacity. All five also sustained two 88.89 Mbit/s streams. Across all BBR
streaming cases, including higher-rate streams with 96 churn workers and the
degraded link, the maximum calculated prebuffer requirement was 0.65 seconds.
None exceeded the modeled two-second playback buffer. These short synthetic
workloads do not establish application playback or multi-hour reliability.

Allocation requests come from **one separate counted pass per allocator**.
They count successful Rust `GlobalAlloc` alloc, alloc_zeroed and realloc calls,
normalized by verified binary GiB. They exclude allocator-internal operations
and non-Rust C allocations. Counting can change scheduling and packetization;
counted timings and RSS are excluded from performance comparisons.

## Saturation and churn

Loopback TCP downloads used 1, 10 and 100 concurrent streams for eight seconds;
duplex used ten echo streams, with verified goodput summed over both directions.
The metro saturation case used two streams for 12 seconds. The interactive probe
ran alongside these workloads.

| Allocator | TCP1, Gbit/s | TCP10, Gbit/s | TCP100, Gbit/s | Duplex10, Gbit/s | Metro bulk, Mbit/s |
| --- | --- | --- | --- | --- | --- |
| Default | 5.60 ± 0.25 | 6.15 ± 0.15 | 5.35 ± 0.14 | 6.87 ± 0.02 | 568.17 ± 4.74 |
| mimalloc v3 | 5.81 ± 0.11 | 6.22 ± 0.14 | 5.39 ± 0.25 | 6.69 ± 0.07 | 566.99 ± 3.01 |
| mimalloc secure | 5.69 ± 0.11 | 6.08 ± 0.07 | 5.29 ± 0.06 | 6.51 ± 0.08 | 568.78 ± 4.38 |
| jemalloc | 5.62 ± 0.14 | 6.02 ± 0.16 | 5.26 ± 0.02 | 6.55 ± 0.06 | 564.62 ± 3.66 |
| tcmalloc | 5.95 ± 0.18 | 6.25 ± 0.10 | 5.34 ± 0.07 | 6.70 ± 0.14 | 566.83 ± 4.37 |

These are application-delivery rates on this host and fixture, not transport
ceilings. Single-stream mimalloc and default run-to-run ranges overlap; the
study does not establish a universal speed improvement.

Churn exercised 32 and 128 concurrent workers, plus 256 workers overloading the
128-flow cap. Each connection performed full application mTLS, PROXY validation,
a timestamped request and a verified 4 KiB echo. Every fourth intentionally
aborted. The table describes the 12-second, 128-worker case. Accepted/s counts
successful setup and PROXY validation; errors count failed attempts, and
censored operations were still in flight at cutoff. Forward p99 describes
delivered requests. Fast failures do not count as successful goodput.

| Allocator | Accepted/s | Goodput, Mbit/s | CPU-seconds/GiB | Forward p99, ms | Allocation requests/GiB | Errors / run | Censored / run |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Default | 3,382 ± 20 | 228.27 ± 1.32 | 54.09 ± 1.53 | 6.887 | 8,973,764 | 2,084 | 111 |
| mimalloc v3 | 3,389 ± 25 | 228.82 ± 1.63 | 54.24 ± 1.79 | 6.731 | 8,988,538 | 1,896 | 109 |
| mimalloc secure | 3,389 ± 19 | 228.83 ± 1.25 | 54.86 ± 1.34 | 6.767 | 8,922,673 | 1,945 | 109 |
| jemalloc | 3,373 ± 42 | 227.72 ± 2.84 | 53.39 ± 1.89 | 6.879 | 8,959,690 | 1,863 | 112 |
| tcmalloc | 3,350 ± 31 | 226.12 ± 2.06 | 54.01 ± 1.55 | 7.035 | 9,106,314 | 1,842 | 110 |

Accepted churn was similar across allocators. Endpoint application mTLS also
consumes shared host CPU, so these rates are not isolated relay setup ceilings.

## Memory retention

RSS was sampled every 25 ms; per-process kernel high-water marks were also
retained. This table uses sampled peaks over each whole loopback stress session,
then RSS after all public clients closed. The authenticated backhaul stayed
alive. There was no forced allocator purge. The 60-second idle wait allowed
the normal 30-second UDP association expiry to run; idle FD counts matched.

| Allocator | Whole-session peak RSS, MiB | RSS after 60s idle, MiB |
| --- | --- | --- |
| Default | 96.9 ± 8.1 | 88.6 ± 9.8 |
| mimalloc v3 | 121.1 ± 1.7 | 30.7 ± 1.4 |
| mimalloc secure | 152.5 ± 4.3 | 41.3 ± 0.4 |
| jemalloc | 157.5 ± 4.3 | 25.8 ± 0.9 |
| tcmalloc | 138.3 ± 4.6 | 136.9 ± 5.3 |

Mimalloc retained about 65% less RSS than default after stress, with a 25% higher
whole-session sampled peak. Default still had the lowest paced-streaming RSS.
Jemalloc saved another 5 MiB after idle but had a higher peak than mimalloc.
RSS sums both processes, including shared pages in each; it is not unique
physical memory. Sampling can miss brief peaks.

TCMalloc's documented maintenance startup helper was invoked, but the built
binary left `MallocExtension_Internal_ProcessBackgroundActions` weak undefined.
Its predicate consequently did not start a maintenance thread. The retention
result describes this default binding configuration, not upstream TCMalloc
with active housekeeping or an optimized cache policy.

## UDP coverage

Eight-second echo saturation used 1,200- and 32,768-byte datagrams with bounded
outstanding windows. Goodput counts verified complete replies and sums both
directions. A separate fresh-port flood exercised association admission and
cleanup; it intentionally exceeded the 128-association limit.

| Allocator | 1,200-byte Gbit/s | CPU-seconds/GiB | Reply delivery | 32,768-byte Gbit/s | CPU-seconds/GiB | Reply delivery |
| --- | --- | --- | --- | --- | --- | --- |
| Default | 1.73 ± 0.02 | 9.87 ± 0.09 | 99.5% | 4.80 ± 0.17 | 3.80 ± 0.09 | 99.6% |
| mimalloc v3 | 1.70 ± 0.07 | 9.97 ± 0.41 | 99.4% | 4.90 ± 0.14 | 3.68 ± 0.06 | 99.6% |
| mimalloc secure | 1.72 ± 0.03 | 9.99 ± 0.17 | 99.4% | 4.74 ± 0.10 | 3.82 ± 0.05 | 99.6% |
| jemalloc | 1.74 ± 0.12 | 9.76 ± 0.71 | 99.4% | 4.71 ± 0.09 | 3.82 ± 0.07 | 99.6% |
| tcmalloc | 1.70 ± 0.12 | 9.96 ± 0.63 | 99.4% | 4.86 ± 0.05 | 3.71 ± 0.05 | 99.6% |

The differences do not identify a universal UDP winner. Port-flood delivery
was approximately 4.3–4.5% under the intentional association-cap overload.

## Security and maintenance

The secure mimalloc build enabled `MI_SECURE=4`: metadata guards and allocation
randomization, encoded free lists, padding checks and additional double-free
checks. Ordinary v3 also separates page metadata from payloads. Secure mode's
paced-streaming CPU mean was about 2.5% higher than plain mimalloc, with overlapping
repeat variation; whole-stress peak and idle RSS were about 26% and 35% higher.
These measurements quantify resource cost, not exploit resistance. Neither
configuration guarantees memory safety or freed-secret erasure.

All variants retained the same authentication, encryption, peer pins, fixed
targets, flow limits and application-TLS behavior. Allocator selection does not
change those protections. Mimalloc adds a maintained native dependency and its
Rust binding; there is no custom allocator implementation in Zerotun.

## Checking later changes

The study used an isolated harness and kept raw investigation artifacts outside
the product repository. The tables above are historical study results; they
are not output from the repository's Criterion suite.

Use the maintained [performance regression tests](benchmarks.md) to compare
changes against a reference commit:

```sh
cargo xtask check
cargo xtask bench-compare HEAD^ -- '^loopback/bbr/download/(1|100)$'
```

That command runs the same current harness against both binaries on one host.
Use an empty output directory for each comparison, as documented in the
benchmark guide. Its deterministic link fixture has no added jitter or random
loss, and its loaded-tail metric is not a p99. It checks refactor regressions;
it does not reproduce this study's full resource and impairment measurements.

The allocator switch passed the CLI smoke suite and four focused BBR regression
gates against `34c98ad`: single-stream and 100-stream downloads, metro downloads,
and loaded interactive latency. An initial short 100-stream run averaged about
10% lower throughput; a longer 40-sample repeat detected no significant change.
Both passed the documented 15% slowdown gate. This variation reinforces the
need for repeated comparisons rather than a universal throughput claim.
