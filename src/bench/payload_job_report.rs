use crate::bench::payload_job_runner::PayloadJobTiming;
use std::io::Write;
use std::path::Path;

const CSV_HEADER: &str = "label,iteration,block_number,job_duration_us,tx_count,gas_used,\
    build_kind,exec_duration_us,trie_root_duration_us";

pub fn write_csv(timings: &[PayloadJobTiming], path: &Path, label: &str) -> eyre::Result<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "{}", CSV_HEADER)?;

    for t in timings {
        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{}",
            label,
            t.iteration,
            t.block_number,
            t.job_duration_us,
            t.tx_count,
            t.gas_used,
            t.build_kind,
            t.exec_duration_us,
            t.trie_root_duration_us,
        )?;
    }

    println!("CSV written to: {}", path.display());
    Ok(())
}

pub fn print_summary(timings: &[PayloadJobTiming], label: &str) {
    if timings.is_empty() {
        println!("No timing data for '{}'", label);
        return;
    }

    println!("\n{}", "=".repeat(60));
    println!("  Payload-Job Summary: {} ({} iterations)", label, timings.len());
    println!("{}\n", "=".repeat(60));

    let job_durations: Vec<u128> = timings.iter().map(|t| t.job_duration_us).collect();
    let exec_durations: Vec<u128> = timings.iter().map(|t| t.exec_duration_us).collect();
    let trie_durations: Vec<u128> = timings.iter().map(|t| t.trie_root_duration_us).collect();
    let tx_counts: Vec<u128> = timings.iter().map(|t| t.tx_count as u128).collect();

    println!(
        "{:<22} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Metric", "Mean", "P50", "P90", "P95", "P99"
    );
    println!("{}", "-".repeat(75));

    let job_stats = compute_stats(&job_durations);
    println!(
        "{:<22} {:>10.0} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
        "job_duration (us)",
        job_stats.mean,
        job_stats.p50,
        job_stats.p90,
        job_stats.p95,
        job_stats.p99
    );

    let exec_stats = compute_stats(&exec_durations);
    println!(
        "{:<22} {:>10.0} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
        "exec_duration (us)",
        exec_stats.mean,
        exec_stats.p50,
        exec_stats.p90,
        exec_stats.p95,
        exec_stats.p99
    );

    let trie_stats = compute_stats(&trie_durations);
    println!(
        "{:<22} {:>10.0} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
        "trie_root (us)",
        trie_stats.mean,
        trie_stats.p50,
        trie_stats.p90,
        trie_stats.p95,
        trie_stats.p99
    );

    let tx_stats = compute_stats(&tx_counts);
    println!(
        "{:<22} {:>10.0} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
        "tx_count", tx_stats.mean, tx_stats.p50, tx_stats.p90, tx_stats.p95, tx_stats.p99
    );

    // Build kind breakdown
    let mut kind_counts = std::collections::HashMap::new();
    for t in timings {
        *kind_counts.entry(t.build_kind.clone()).or_insert(0usize) += 1;
    }
    println!("\n  Build kind breakdown:");
    for (kind, count) in &kind_counts {
        println!("    {}: {} ({:.1}%)", kind, count, *count as f64 / timings.len() as f64 * 100.0);
    }

    // Success rate
    let successful = timings.iter().filter(|t| t.tx_count > 0).count();
    println!(
        "\n  Success rate: {}/{} ({:.1}%)",
        successful,
        timings.len(),
        successful as f64 / timings.len() as f64 * 100.0
    );

    let total_gas: u64 = timings.iter().map(|t| t.gas_used).sum();
    let total_txs: usize = timings.iter().map(|t| t.tx_count).sum();
    println!("  Total gas: {}", total_gas);
    println!("  Total txs: {}", total_txs);
}

struct Stats {
    mean: f64,
    p50: f64,
    p90: f64,
    p95: f64,
    p99: f64,
}

fn compute_stats(values: &[u128]) -> Stats {
    if values.is_empty() {
        return Stats { mean: 0.0, p50: 0.0, p90: 0.0, p95: 0.0, p99: 0.0 };
    }

    let mut sorted: Vec<u128> = values.to_vec();
    sorted.sort_unstable();

    let n = sorted.len();
    let sum: u128 = sorted.iter().sum();
    let mean = sum as f64 / n as f64;

    Stats {
        mean,
        p50: percentile(&sorted, 50.0),
        p90: percentile(&sorted, 90.0),
        p95: percentile(&sorted, 95.0),
        p99: percentile(&sorted, 99.0),
    }
}

fn percentile(sorted: &[u128], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (pct / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    let idx = idx.min(sorted.len() - 1);
    sorted[idx] as f64
}
