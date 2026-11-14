//! Regression-gate tests using controlled Criterion estimates and workload metadata.

#[path = "../benches/support/regression.rs"]
mod regression;

use std::fs;
use std::path::Path;

use serde_json::json;

fn estimates(root: &Path, name: &str, low: f64, high: f64, confidence: f64) {
    let directory = root.join("tcp").join(name);
    fs::create_dir_all(&directory).expect("create Criterion fixture directory");
    fs::write(
        directory.join("estimates.json"),
        json!({"mean": {"confidence_interval": {
            "lower_bound":low,"upper_bound":high,"confidence_level":confidence
        }}})
        .to_string(),
    )
    .expect("write Criterion fixture");
    fs::write(
        directory.join("benchmark.json"),
        json!({
            "full_id":"tcp/download","throughput":{"Bytes":1024}
        })
        .to_string(),
    )
    .expect("write Criterion fixture");
}

#[test]
fn permits_noise_and_improvements_but_rejects_confident_slowdowns() {
    let d = tempfile::tempdir().expect("create benchmark fixture directory");
    estimates(d.path(), "reference", 90., 110., 0.99);
    for (low, high) in [(80., 90.), (105., 135.), (120., 125.)] {
        estimates(d.path(), "new", low, high, 0.99);
        regression::check(d.path(), "reference", 0.15).expect("permitted slowdown passes");
    }
    estimates(d.path(), "new", 130., 140., 0.99);
    assert!(
        regression::check(d.path(), "reference", 0.15)
            .expect_err("confident slowdown is rejected")
            .to_string()
            .contains("performance regressions")
    );
}

#[test]
fn missing_or_incomparable_results_cannot_pass() {
    let d = tempfile::tempdir().expect("create benchmark fixture directory");
    assert!(regression::check(d.path(), "reference", 0.15).is_err());
    estimates(d.path(), "reference", 90., 110., 0.99);
    assert!(regression::check(d.path(), "reference", 0.15).is_err());
    fs::remove_file(d.path().join("tcp/reference/estimates.json"))
        .expect("remove reference estimate");
    estimates(d.path(), "new", 90., 110., 0.99);
    assert!(regression::check(d.path(), "reference", 0.15).is_err());
    estimates(d.path(), "reference", 90., 110., 0.99);
    estimates(d.path(), "new", 90., 110., 0.95);
    assert!(regression::check(d.path(), "reference", 0.15).is_err());
    estimates(d.path(), "new", 90., 110., 0.99);
    fs::write(
        d.path().join("tcp/new/benchmark.json"),
        json!({
            "full_id":"tcp/download","throughput":{"Bytes":512}
        })
        .to_string(),
    )
    .expect("write Criterion fixture");
    assert!(regression::check(d.path(), "reference", 0.15).is_err());
    assert!(regression::check(d.path(), "../reference", 0.15).is_err());
    assert!(regression::check(d.path(), "reference", -0.1).is_err());
}
