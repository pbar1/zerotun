# Performance regression tests

`cargo xtask bench` runs Criterion against actual release-mode
edge and origin processes. It exercises the public CLI rather than a separate
copy of the forwarding implementation, so refactors remain covered without a
public library API or test-only runtime hooks. It needs no capabilities, external
services, Python, OpenSSL executable, TUN devices, routes or firewall changes.

Both CUBIC and BBR cover:

- New public TCP connection, application mTLS handshake and first response over
  an already authenticated backhaul, for IPv4 and IPv6. This includes target
  dialing and PROXY delivery; it is not a cold QUIC-handshake measurement.
- Interactive 128-byte request/response on a warm connection.
- Verified origin-to-edge downloads with 1, 2, 10 and 100 concurrent connections.
- Two concurrent bidirectional echo streams. Throughput counts both directions.
- Opaque UDP echo at 1,200 and 32,768 bytes (8,192 on macOS), including relay
  fragmentation. UDP throughput counts request and reply payloads, with strict
  packet validation.
- Two simultaneous downloads over a bounded local UDP link fixture: 15 ms RTT,
  940 Mbit/s per direction, 1,500-byte IPv4 MTU. Delay is split once into 7.5 ms
  each way; capacity counts UDP payload plus 28 bytes of IPv4/UDP headers.
- A tail-sensitive interactive metric while two verified downloads saturate
  that link. `loaded_tail_8` reports the average of each eight-request window's
  maximum latency. It is deliberately **not labeled p99**.

The fixture enables `--proxy-tcp`, so every TCP connection verifies the original
source/destination in the backend's PROXY v2 header. It then requires end-to-end
TLS 1.3 with client and server certificates, using a CA separate from the backhaul
CA. Application TLS resumption is disabled for consistent connection measurements.
Credentials are generated in a temporary private directory and removed with the
fixture; relays receive only backhaul identities. Download timing counts verified
received bytes, not bytes merely queued for transmission. Warm bulk streams exclude handshakes;
all relevant setup is included in the connection benchmark.

Backhaul credentials use ML-DSA-65; application TLS uses separate ECDSA credentials
to exercise transparent forwarding independently of the backhaul's cryptographic policy.

The link fixture is a regression workload, not an Internet simulator or tc netem.
It uses bounded queues and ordinary UDP sockets, with no artificial jitter or
random loss. Queue overflow/send failure invalidates a run. Kernel socket limits,
scheduling and same-host CPU contention can still affect it. All processes and
the fixture share one machine. These tests do not requalify the historical
[WAN measurements](performance.md), certify a universal gigabit ceiling, or
assert CPU/RSS limits and global p99 latency.

## A gate that fails on regressions

Criterion's normal baseline comparison prints statistics; it does not fail the
build for slower code. Use the comparison wrapper for an enforced check:

```sh
cargo xtask bench-compare HEAD^
# Focus on one workload while developing:
ZEROTUN_BENCH_OUTPUT="$PWD/target/criterion/focused" \
  cargo xtask bench-compare HEAD^ -- '^metro/bbr/download_2_streams$'
```

The task builds the reference commit in a temporary detached worktree with a
separate cached target directory, and freezes the candidate binary from the
current checkout. Both use `xtask build --release`; `xtask bench` runs the current
harness against each binary on the same host, with identical workloads and settings. The
reference need not contain benchmark code. This avoids comparing changed
fixtures or relying on committed hardware-specific scores.

The reference must support the current backhaul credential policy. Pre-PQ
binaries cannot load this harness's ML-DSA-65 identities; establish a new baseline
at the first PQ-capable commit rather than comparing across that transition.

The default gate fails when the candidate's **entire 99% confidence interval for
mean operation time is more than 15% slower than the reference's entire interval**:

```text
candidate.mean.lower_bound > reference.mean.upper_bound × 1.15
```

For a fixed payload, longer duration means lower throughput. The budget is an
increase in time, not an identically sized decrease in throughput. The check
also rejects missing results, changed benchmark definitions, invalid estimates
and confidence levels below 99%. `loaded_tail_8` applies the same check to its
tail-sensitive metric, rather than claiming to estimate a global p99.

Set `ZEROTUN_BENCH_MAX_SLOWDOWN=0.10` for a stricter 10% time budget. Each case
normally uses 20 samples, one-second warmup and three-second target measurement.
Expensive cases can exceed that target to collect enough samples. Calibration
uses elapsed wall time even when `loaded_tail_8` returns its custom metric.
Changing Criterion's sample/timing flags affects both sides of the wrapper.

Confidence intervals handle sampling noise, not arbitrary changes in host load.
Run benchmarks sequentially without concurrent builds or other heavy workloads.
Use a stable machine for small regression budgets; a noisy hosted runner may
require a rerun before diagnosing a code regression. Absolute throughput and
latency floors require a specified, controlled machine and network.
For a release or a longer refactor, compare with a fixed known-good commit;
per-commit budgets do not prevent small slowdowns from accumulating.

## Outputs and CI

All estimates, raw Criterion samples and provenance go under ignored `target/`;
no scores, credentials or investigation logs are committed. The default comparison
output is `target/criterion/compare/`. Override `ZEROTUN_BENCH_OUTPUT` to move it.
The wrapper requires an empty output directory to prevent stale results from
passing the check. It records commit IDs, binary hashes, compiler and host details.
Temporary source worktrees are removed even on failure.

The [CI workflow](../.github/workflows/ci.yml) runs
`cargo xtask bench -- --test` on amd64 and arm64 to validate
fixtures and payloads once, without timing thresholds. The separate
[Performance workflow](../.github/workflows/performance.yml) compares runtime and
benchmark changes on pushes to `main` with the previous branch head, and PRs with
their base, using one amd64 runner. It can also be started manually with a
reference ref and retains Criterion outputs as CI artifacts for seven days.
Both workflows cache Cargo dependencies. Performance results use the runner's
temporary directory, outside the build cache, and both binaries use the same
selected toolchain. Both checks can be run locally. Container builds include
benchmark/test source for Cargo target discovery; the distroless runtime image
contains the production binary and its runtime libraries.

For a local report without a failure gate, normal Criterion baselines also work:

```sh
cargo xtask bench -- --save-baseline before
# Make a change, then:
cargo xtask bench -- --baseline before
```

`ZEROTUN_BENCH_BIN=/absolute/path/to/zerotun` selects a different CLI binary for
fixture development. The comparison wrapper supplies it automatically. The
regression checker has unit tests for slowdowns, noise, improvements and invalid
or missing result sets.
