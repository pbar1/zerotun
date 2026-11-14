Use the [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)
as a baseline; repository configuration takes precedence.

Enforcement: [rustfmt.toml](rustfmt.toml), [clippy.toml](clippy.toml), workspace
lints in [Cargo.toml](Cargo.toml), and [CI](.github/workflows/ci.yml).
Run `cargo xtask check`; see `cargo xtask --help` for individual tasks.
