use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde_json::Value;

fn read(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn mean(path: &Path) -> Result<(f64, f64)> {
    let value = read(path)?;
    let ci = &value["mean"]["confidence_interval"];
    let lower = ci["lower_bound"]
        .as_f64()
        .context("missing lower mean bound")?;
    let upper = ci["upper_bound"]
        .as_f64()
        .context("missing upper mean bound")?;
    let confidence = ci["confidence_level"]
        .as_f64()
        .context("missing confidence level")?;
    ensure!(
        lower > 0. && lower.is_finite() && upper >= lower && upper.is_finite(),
        "invalid duration estimate"
    );
    ensure!(
        (0.99..=1.0).contains(&confidence),
        "regression gate requires 99% confidence intervals"
    );
    Ok((lower, upper))
}

fn visit(root: &Path, baseline: &str, cases: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().is_some_and(|n| n == baseline) {
            cases.push(path.parent().context("baseline parent")?.to_path_buf());
        } else if !matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("new" | "change" | "report")
        ) {
            visit(&path, baseline, cases)?;
        }
    }
    Ok(())
}

// Fail conservatively: the candidate's entire 99% mean-duration interval must
// be beyond the allowed slowdown relative to the reference's entire interval.
// For fixed payloads a longer duration is a throughput regression as well.
pub fn check(root: &Path, baseline: &str, max_slowdown: f64) -> Result<()> {
    ensure!(
        !baseline.is_empty()
            && !baseline.contains(['/', '\\'])
            && !matches!(baseline, "." | ".." | "new" | "change" | "report"),
        "invalid baseline name"
    );
    ensure!(
        max_slowdown.is_finite() && (0.0..=1.0).contains(&max_slowdown),
        "slowdown must be a fraction in 0..=1"
    );
    let mut cases = Vec::new();
    visit(root, baseline, &mut cases)?;
    ensure!(
        !cases.is_empty(),
        "no reference benchmarks found in {}",
        root.display()
    );
    let mut failures = Vec::new();
    for case in &cases {
        let reference = case.join(baseline);
        let candidate = case.join("new");
        let expected = read(&reference.join("benchmark.json"))?;
        let observed = read(&candidate.join("benchmark.json"))?;
        ensure!(
            expected == observed,
            "benchmark definition changed: {}",
            case.display()
        );
        let (_, reference_upper) = mean(&reference.join("estimates.json"))?;
        let (candidate_lower, _) = mean(&candidate.join("estimates.json"))?;
        if candidate_lower > reference_upper * (1. + max_slowdown) {
            failures.push(format!(
                "{}: at least {:.1}% slower",
                expected["full_id"].as_str().unwrap_or("unknown"),
                (candidate_lower / reference_upper - 1.) * 100.
            ));
        }
    }
    if !failures.is_empty() {
        bail!("performance regressions:\n{}", failures.join("\n"));
    }
    eprintln!(
        "Performance gate passed: {} cases, {:.0}% slowdown budget, 99% intervals",
        cases.len(),
        max_slowdown * 100.
    );
    Ok(())
}
