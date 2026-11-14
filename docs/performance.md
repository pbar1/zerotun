# WAN measurements

Measured using a pre-release Quinn implementation before the CLI cleanup.
These numbers describe that build and path; they are not measurements of the
current release.

The edge had one vCPU / 1 GiB. Both relays ran unprivileged on Linux across a WAN
path. The origin forwarded PROXY v2 to a separate backend, where application
mTLS terminated. No TUN, host routes or networking capabilities were needed.

Capacity trials verified application payloads, warmed up for two seconds, then
counted received bytes for 15 seconds. Each condition had three trials; ± is
sample standard deviation. Loaded latency used a separate interactive connection.

| Path / controller | Bulk streams | Mbit/s | Mean trial p99 |
| --- | ---: | ---: | ---: |
| Edge-local client, BBR | 2 | 864.5 ± 22.5 | 66.9 ms |
| Edge-local client, CUBIC | 2 | 857.2 ± 24.3 | 53.2 ms |
| Remote client, BBR | 2 | 369.7 ± 3.3 | 96.9 ms |
| Remote client, CUBIC | 2 | 369.4 ± 22.8 | 73.9 ms |
| Direct TCP/mTLS control, remote client | 2 | 365.8 ± 46.9 | 134.2 ms |

Edge-local clients exercised the backhaul once; the remote-client path crossed
the WAN twice and included access-network limitations. Native TCP/mTLS controls
from the origin averaged 910.5 Mbit/s upload and 894.3 Mbit/s download. Backhaul
RTT was about 10 ms.

Both controllers used 4 MiB stream windows, 32 MiB connection/send windows and
2 MiB socket-buffer requests. CUBIC was the default when these measurements were
recorded. These sequential trials showed similar throughput and somewhat lower
loaded latency for CUBIC. They do not establish a universal advantage for either
controller. BBR is now the default; both controllers retain the same tuned
windows and socket-buffer requests.

The host's 212,992-byte socket limits capped the requests. One preliminary BBR run
at those limits delivered 751 Mbit/s with about 30,000 UDP receive-buffer drops.
After raising both limits to 2,097,152 bytes, all three subsequent edge-local BBR
trials recorded zero such drops. This was not a matched repeated comparison.
Edge relay CPU was about 0.41–0.46 cores and RSS about 82 MiB during those trials;
origin CPU was about 0.24–0.26 cores and RSS 20–23 MiB. RSS excludes socket buffers
and sandbox overhead.

Two simultaneous paced streams passed at both 22.22 and 88.89 Mbit/s
per stream, in repeated 20-second runs. Worst required prebuffer was 0.14 and
0.33 seconds respectively. A two-minute public-client saturation soak verified
4.51 GB with no zero-delivery seconds. This does not establish multi-hour reliability.

IPv4/IPv6 PROXY metadata, application certificate isolation, exact peer pins,
verified transfers and UDP echoes passed. A roughly 45-second blackout dropped
existing connections; new authenticated requests recovered 1.85 seconds after
the restore command. Starting the origin before the edge recovered in 1.22 seconds
after edge startup. Those timings include control overhead and used the former
200 ms reconnect delay; the current implementation retries every second.

IPv4/IPv6 UDP were tested separately. Linux edge-local ingress passed datagrams
through 60,000 bytes over the backhaul; this does not establish delivery of large
fragmented datagrams from arbitrary Internet clients. Actual HTTP/3, application
Gateway policies, certificate rotation and rolling updates were not qualified.

The evaluated binary used Rust 1.100.0 and had SHA-256
`dd8084f416ea8beddc8008bf7728fe60428ce1c64b3c54695dd54da0cfe9bef2`.
