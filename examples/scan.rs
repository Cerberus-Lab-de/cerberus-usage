//! Real-data check for the log scanner: `cargo run --release --example scan [root]`.
//! Prints only counts, durations and model names. State goes to the temp dir.

use std::path::PathBuf;
use std::time::Instant;

use cerberus_usage::log_scan::{ScanState, load_state, save_state, scan};

fn main() -> std::io::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
                .join(".claude")
                .join("projects")
        });
    let state_path = std::env::temp_dir().join("cerberus-usage-scan-example.json");
    let _ = std::fs::remove_file(&state_path);

    let mut state = ScanState::default();
    let t = Instant::now();
    let stats = scan(&root, &mut state, &|_, _| {})?;
    let first = t.elapsed();
    save_state(&state_path, &state)?;
    println!("run 1: {first:?}");
    println!(
        "  files={} lines={} usage_lines={} unique_ids={} ratio={:.2} bytes={} failed={}",
        stats.files_total,
        stats.lines_read,
        stats.usage_lines,
        state.records.len(),
        stats.usage_lines as f64 / state.records.len().max(1) as f64,
        stats.bytes_read,
        stats.files_failed,
    );
    println!("  quota_events={}", state.quota.len());

    let mut models: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in state.records.values() {
        *models.entry(r.model.as_str()).or_default() += 1;
    }
    println!("  models: {models:?}");

    let t = Instant::now();
    let mut state = load_state(&state_path);
    let loaded = t.elapsed();
    let t = Instant::now();
    let stats = scan(&root, &mut state, &|_, _| {})?;
    println!(
        "run 2: load_state={loaded:?} scan={:?} bytes_read={} new_records={} files_read={}",
        t.elapsed(),
        stats.bytes_read,
        stats.new_records,
        stats.files_read,
    );
    println!(
        "  state file size: {} bytes",
        std::fs::metadata(&state_path)?.len()
    );
    Ok(())
}
