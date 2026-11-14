# zerotun

Publish services without exposing your server's IP.

Forward TCP and UDP through a public edge, with no port forwarding required
on your network.

```text
╭────────────────╮   ╭────────────────╮
│    Clients     │──▶│  Public edge   │
╰────────────────╯   ╰────────────────╯
                              ▲
···· Firewall ················│········
                              │
╭────────────────╮   ╭────────────────╮
│  Your service  │◀──│ Private origin │
╰────────────────╯   ╰────────────────╯
```

## Install

Linux amd64 and arm64 binaries are published on the
[releases page](https://github.com/pbar1/zerotun/releases).

To build from source on Linux or macOS, with Rust and a C compiler:

```sh
cargo build --release --locked
```

The binary is `target/release/zerotun`.

## Usage

With [relay certificates](docs/certificates.md) in the working directory:

On the public edge:

```sh
zerotun edge \
  --listen-tcp '[::]:443' --listen-udp '[::]:443' \
  --bind '[::]:9443' \
  --ca ca.pem --cert edge.pem --key edge.key --peer-cert origin.pem
```

At the origin:

```sh
zerotun origin \
  --edge '<edge-public-ip>:9443' --server-name edge.example.com \
  --target-tcp 127.0.0.1:443 --target-udp 127.0.0.1:443 \
  --ca ca.pem --cert origin.pem --key origin.key --peer-cert edge.pem
```

Clients connect to the edge on TCP or UDP port `443`; the service is at
`127.0.0.1:443` on the origin. The backhaul uses UDP port `9443`.

Use `zerotun edge --help` or `zerotun origin --help` for all options.

## PROXY protocol

To send original client addresses to a PROXY-aware backend, add `--proxy-tcp`
or `--proxy-udp` at the origin. Both are disabled by default.

## Documentation

- [Certificates](docs/certificates.md)
- [Networking and options](docs/networking.md)
- [Containers](docs/containers.md)
- [WAN measurements](docs/performance.md)
- [Allocator comparison](docs/allocators.md)
- [Development](docs/development.md) and [benchmarks](docs/benchmarks.md)
