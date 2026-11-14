//! Shared development and CI commands.

mod bench;

use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use clap::Parser;
use clap::Subcommand;
use duct::cmd;

#[derive(Parser)]
#[command(name = "xtask", about = "Development and CI checks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Format the workspace.
    Fmt {
        /// Check formatting without changing files.
        #[arg(long)]
        check: bool,
    },
    /// Run Clippy for all targets and features.
    Lint,
    /// Check documentation.
    Doc,
    /// Build all workspace packages.
    Build {
        #[arg(long)]
        release: bool,
        #[arg(long)]
        manifest_path: Option<PathBuf>,
    },
    /// Run tests.
    Test,
    /// Check formatting, lints, documentation, and tests.
    Check,
    /// Run Criterion; pass benchmark arguments after --.
    Bench {
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Compare benchmarks against a Git revision.
    BenchCompare {
        reference: OsString,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Check that a release binary's version matches its tag.
    CheckVersion { binary: PathBuf, tag: String },
}

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask must be inside the workspace")?;
    env::set_current_dir(root)?;
    run(Cli::parse().command)
}

fn run(task: Task) -> Result<()> {
    let command = match task {
        Task::Check => {
            for check in [Task::Fmt { check: true }, Task::Lint, Task::Doc, Task::Test] {
                run(check)?;
            }
            return Ok(());
        }
        Task::Fmt { check } => {
            let command = if check {
                cmd!("cargo", "fmt", "--all", "--", "--check")
            } else {
                cmd!("cargo", "fmt", "--all")
            };
            command.dir(".cargo/fmt").env_remove("RUSTUP_TOOLCHAIN")
        }
        Task::Lint => cmd!(
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ),
        Task::Doc => {
            let command = cmd!(
                "cargo",
                "doc",
                "--workspace",
                "--all-features",
                "--no-deps",
                "--document-private-items",
                "--locked",
            );
            env::var_os("CARGO_ENCODED_RUSTDOCFLAGS").map_or_else(
                || {
                    let mut flags = env::var_os("RUSTDOCFLAGS").unwrap_or_default();
                    flags.push(" -D warnings");
                    command.env("RUSTDOCFLAGS", flags)
                },
                |mut flags| {
                    if !flags.is_empty() {
                        flags.push("\u{1f}");
                    }
                    flags.push("-D\u{1f}warnings");
                    command.env("CARGO_ENCODED_RUSTDOCFLAGS", flags)
                },
            )
        }
        Task::Build {
            release,
            manifest_path,
        } => {
            let mut args: Vec<OsString> = ["build", "--workspace", "--locked"]
                .map(OsString::from)
                .into();
            if release {
                args.push("--release".into());
            }
            if let Some(path) = manifest_path {
                args.extend([OsString::from("--manifest-path"), path.into_os_string()]);
            }
            cmd("cargo", args)
        }
        Task::Test => cmd!("cargo", "test", "--workspace", "--all-features", "--locked"),
        Task::Bench { args } => cmd(
            "cargo",
            ["bench", "--locked", "--bench", "relay", "--"]
                .into_iter()
                .map(OsString::from)
                .chain(args),
        ),
        Task::BenchCompare { reference, args } => {
            return bench::compare(reference, &args);
        }
        Task::CheckVersion { binary, tag } => {
            let version = tag
                .strip_prefix('v')
                .context("release tag must start with v")?;
            let actual = cmd!(&binary, "--version")
                .read()
                .with_context(|| format!("read version from {}", binary.display()))?;
            let actual = actual.trim();
            ensure!(
                actual == format!("zerotun {version}"),
                "release tag {tag} does not match binary version: {actual}"
            );
            return Ok(());
        }
    };
    eprintln!("> {command:?}");
    command.run()?;
    Ok(())
}
