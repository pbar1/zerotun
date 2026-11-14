# Development

Use Rust and a C compiler on Linux or macOS. With Nix, `nix develop` enters the
development shell.

```sh
cargo xtask check
```

This checks formatting, Clippy, documentation and tests. The formatter uses the
nightly toolchain selected by `.cargo/fmt/rust-toolchain.toml`; compilation uses
the stable toolchain in `rust-toolchain.toml`.

Format with `cargo xtask fmt`. See `cargo xtask --help` for individual tasks.
Run the end-to-end Criterion suite with `cargo xtask bench`, or compare against
a reference commit with `cargo xtask bench-compare HEAD^`. See the
[benchmark guide](benchmarks.md) for coverage, outputs and regression policy.
Generated results stay under ignored `target/`.

The [CI workflow](../.github/workflows/ci.yml) checks native amd64 and arm64
runners and tests the container-built binary. The
[performance workflow](../.github/workflows/performance.yml) compares runtime
and benchmark changes against the previous branch head or PR base. Tagged
releases run tests before publishing glibc binaries, SHA-256 checksums and
signed container images.
