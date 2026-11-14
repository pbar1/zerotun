# Containers

Version tags (`v*`) publish `ghcr.io/pbar1/zerotun:<tag>` for Linux amd64 and arm64.
The image uses distroless with glibc and runs as UID/GID 65532. It needs no shell,
system CA bundle or writable filesystem.

## Run

For example, on a Linux origin with certificates in `/etc/zerotun`:

```sh
version=vX.Y.Z
docker run --rm --read-only --cap-drop ALL \
  --security-opt no-new-privileges \
  -v /etc/zerotun:/certs:ro "ghcr.io/pbar1/zerotun:$version" origin \
  --edge '<edge-public-ip>:9443' --server-name edge.example.com \
  --target-tcp '<target-ip>:<target-port>' --target-udp '<target-ip>:<target-port>' \
  --ca /certs/ca.pem --cert /certs/origin.pem --key /certs/origin.key \
  --peer-cert /certs/edge.pem
```

Replace `vX.Y.Z` with a release tag and `<edge-public-ip>` with your edge's IP.
Mounted private keys must be readable by UID/GID 65532.

## Verify an image

Release images are signed with Cosign:

```sh
version=vX.Y.Z
cosign verify "ghcr.io/pbar1/zerotun:$version" \
  --certificate-identity \
    "https://github.com/pbar1/zerotun/.github/workflows/release.yml@refs/tags/$version" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Build

```sh
docker build -t zerotun .
```

For both architectures:

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t YOUR_IMAGE --push .
```

Builds use cargo-chef dependency layers, cached separately for each architecture.
