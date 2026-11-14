use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use duct::cmd;

pub fn compare(mut reference: OsString, args: &[OsString]) -> Result<()> {
    reference.push("^{commit}");
    let reference = cmd!(
        "git",
        "rev-parse",
        "--verify",
        "--end-of-options",
        reference
    )
    .read()?;
    let target =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from("target"), PathBuf::from);
    fs::create_dir_all(&target)?;
    let target = fs::canonicalize(target)?;
    let report = env::var_os("ZEROTUN_BENCH_OUTPUT")
        .map_or_else(|| target.join("criterion/compare"), PathBuf::from);
    fs::create_dir_all(&report)?;
    ensure!(
        fs::read_dir(&report)?.next().is_none(),
        "use an empty ZEROTUN_BENCH_OUTPUT directory to avoid stale results"
    );
    let report = fs::canonicalize(report)?;
    let temporary = tempfile::tempdir()?;
    let worktree = temporary.path().join("reference");
    let task = env::current_exe()?;
    cmd!("git", "worktree", "add", "--detach", &worktree, &reference).run()?;
    let result = (|| {
        let reference_target = target.join("bench-reference");
        cmd!(
            &task,
            "build",
            "--release",
            "--manifest-path",
            worktree.join("Cargo.toml"),
        )
        .env("CARGO_TARGET_DIR", &reference_target)
        .run()
        .with_context(|| format!("build reference commit {reference}"))?;
        cmd!(&task, "build", "--release")
            .env("CARGO_TARGET_DIR", &target)
            .run()?;
        let candidate = temporary.path().join("candidate");
        fs::copy(target.join("release/zerotun"), &candidate)?;
        let reference_binary = reference_target.join("release/zerotun");
        let metadata = format!(
            "reference={reference}\ncandidate={}\n{}\n{}\n{} {}\n\
             reference-blob={}\ncandidate-blob={}\n",
            cmd!("git", "rev-parse", "HEAD").read()?,
            cmd!("git", "status", "--short").read()?,
            cmd!("rustc", "-Vv").read()?,
            env::consts::OS,
            env::consts::ARCH,
            cmd!("git", "hash-object", &reference_binary).read()?,
            cmd!("git", "hash-object", &candidate).read()?,
        );
        fs::write(report.join("run-info.txt"), metadata)?;
        let bench = |mode| {
            cmd(
                &task,
                ["bench", "--", mode, "reference"]
                    .into_iter()
                    .map(OsString::from)
                    .chain(args.iter().cloned()),
            )
            .env("CARGO_TARGET_DIR", &target)
            .env("ZEROTUN_BENCH_OUTPUT", &report)
        };
        bench("--save-baseline")
            .env("ZEROTUN_BENCH_BIN", &reference_binary)
            .env_remove("ZEROTUN_BENCH_COMPARE")
            .run()?;
        remove_new(&report)?;
        bench("--baseline")
            .env("ZEROTUN_BENCH_BIN", &candidate)
            .env("ZEROTUN_BENCH_COMPARE", "reference")
            .run()?;
        Ok(())
    })();
    let cleanup = cmd!("git", "worktree", "remove", "--force", &worktree)
        .run()
        .map(|_| ())
        .context("remove reference worktree");
    result.and(cleanup)
}

// Candidate results must be fresh: Criterion also writes reference samples to `new`.
fn remove_new(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if entry.file_name() == "new" {
                fs::remove_dir_all(entry.path())?;
            } else {
                remove_new(&entry.path())?;
            }
        }
    }
    Ok(())
}
