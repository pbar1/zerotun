# Networking and options

The origin makes an outbound, mutually authenticated QUIC connection to the edge.
The edge accepts one pinned origin and opens a bidirectional QUIC stream for
each public TCP connection. UDP uses unreliable QUIC DATAGRAM frames and
preserves packet boundaries, including empty packets. Application bytes pass
through unchanged unless PROXY protocol is enabled.

Neither relay needs root, TUN devices, host routes or networking capabilities.
Use high listener ports unless the host already permits unprivileged low ports.

## Addresses

Edge's `--listen-tcp` accepts client TCP traffic; `--bind` listens for the QUIC
backhaul, defaulting to `127.0.0.1:9443`. Neither `--listen-tcp` nor `--listen-udp`
has a default; set one or both. UDP ingress uses a separate socket.
The origin chooses an ephemeral QUIC port and a socket family matching `--edge`.

At the origin, set one or both of `--target-tcp` and `--target-udp`.
Only the configured protocols are forwarded.

All network addresses are single numeric IP:port pairs. `--server-name` is for
certificate verification.

| Bind address | Accepted traffic |
| --- | --- |
| `127.0.0.1:PORT` | IPv4 loopback |
| `[::1]:PORT` | IPv6 loopback |
| `0.0.0.0:PORT` | All IPv4 interfaces |
| `[::]:PORT` | All IPv4 and IPv6 interfaces, on one dual-stack socket |

These rules apply to TCP ingress, UDP ingress and the edge backhaul. IPv6-only
behavior is set explicitly for specific IPv6 addresses, and disabled for `[::]`.
IPv4-mapped client addresses are normalized to IPv4 in connection metadata.
Targets can independently use IPv4 or IPv6, for example `[::1]:443`.

UDP wildcard binds capture each packet's actual destination IP and send replies
from that address using unprivileged packet-info socket APIs. UDP backends see
the origin's socket address.

## PROXY protocol

`--proxy-tcp` prepends PROXY v2 before application bytes, including before a TLS
handshake. `--proxy-udp` prepends PROXY v2 to every UDP request; replies must remain
unprefixed. Both flags belong on the origin and are disabled by default.
`--proxy-tcp` requires `--target-tcp`; `--proxy-udp` requires `--target-udp`.

Targets are fixed at startup; PROXY metadata never selects a dial target.
The edge derives client addresses from its sockets. PROXY metadata is trusted
only as far as the edge is trusted; it does not authenticate clients.
Restrict backend access to the origin. Where practical, restrict the backhaul
UDP port to the origin's IP.

## Limits and recovery

Both modes accept these options; see `--help` for the full CLI reference.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--congestion cubic\|bbr` | `bbr` | QUIC congestion controller |
| `--max-connections` | `128` | Separate TCP and UDP association limits, 1–4096 |
| `--idle-timeout` | `30` | Flow and backhaul idle timeout, seconds |
| `--connect-timeout` | `3` | Handshake, stream-open, target-connect and header timeout, seconds |

The origin retries disconnected backhauls every second. New traffic recovers;
existing TCP connections and UDP associations are discarded. With no backhaul,
the edge closes new TCP connections and drops UDP. SIGINT and SIGTERM close the
backhaul and stop the process; active flows are interrupted.

The flow limits, copy buffers, QUIC windows, UDP queues and reassembly are bounded.
They are not a process memory limit. UDP payloads are capped at 65,507 bytes;
fragment loss drops the packet, and incomplete reassembly expires within two
seconds. Large Internet UDP packets still depend on the public path's MTU.

## Socket buffers and flow control

BBR is the default on both relays; use `--congestion cubic` to select CUBIC.
Both use 4 MiB stream receive windows, 32 MiB connection receive/send windows,
and request 2 MiB UDP socket buffers. UDP forwarding also gets 4 MiB QUIC datagram
send/receive buffers when enabled.

Linux socket limits can cap those requests. Check the actual sizes logged at
startup. In WAN testing, raising `net.core.rmem_max` and `net.core.wmem_max` to
`2097152` eliminated observed receive-buffer drops. Zerotun does not change
sysctls. See [WAN measurements](performance.md).
