use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// Timing data for a single block production.
#[derive(Debug, Clone)]
pub struct BlockTiming {
    pub block_number: u64,
    pub validator_index: usize,
    pub tx_count: usize,
    pub gas_used: u64,
    // Phase timings (microseconds)
    pub state_setup_us: u128,
    pub pre_execution_us: u128,
    pub tx_execution_us: u128,
    pub post_execution_us: u128,
    pub merge_transitions_us: u128,
    pub hashed_state_us: u128,
    pub triedb_convert_us: u128,
    pub triedb_root_us: u128,
    pub block_assembly_us: u128,
    pub finish_us: u128,
    pub total_us: u128,
    // State metrics
    pub hashed_accounts: usize,
    pub hashed_storage_slots: usize,
    pub has_difflayer: bool,
    pub has_cached_reads: bool,
}

const CSV_HEADER: &str = "block_number,validator_index,tx_count,gas_used,\
    state_setup_us,pre_execution_us,tx_execution_us,post_execution_us,\
    merge_transitions_us,hashed_state_us,triedb_convert_us,triedb_root_us,\
    block_assembly_us,finish_us,total_us,\
    hashed_accounts,hashed_storage_slots,has_difflayer,has_cached_reads";

/// Write timing results to a CSV file.
pub fn write_csv(timings: &[BlockTiming], path: &Path, label: &str) -> eyre::Result<()> {
    let mut file = std::fs::File::create(path)?;

    // Write header
    writeln!(file, "label,{}", CSV_HEADER)?;

    for t in timings {
        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            label,
            t.block_number,
            t.validator_index,
            t.tx_count,
            t.gas_used,
            t.state_setup_us,
            t.pre_execution_us,
            t.tx_execution_us,
            t.post_execution_us,
            t.merge_transitions_us,
            t.hashed_state_us,
            t.triedb_convert_us,
            t.triedb_root_us,
            t.block_assembly_us,
            t.finish_us,
            t.total_us,
            t.hashed_accounts,
            t.hashed_storage_slots,
            t.has_difflayer,
            t.has_cached_reads,
        )?;
    }

    println!("CSV written to: {}", path.display());
    Ok(())
}

/// Read timing results from a CSV file.
pub fn read_csv(path: &Path) -> eyre::Result<Vec<BlockTiming>> {
    let content = std::fs::read_to_string(path)?;
    let mut timings = Vec::new();
    let mut lines = content.lines();

    // Skip header
    lines.next();

    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 20 {
            continue;
        }
        // Skip label field (index 0)
        let t = BlockTiming {
            block_number: fields[1].parse().unwrap_or(0),
            validator_index: fields[2].parse().unwrap_or(0),
            tx_count: fields[3].parse().unwrap_or(0),
            gas_used: fields[4].parse().unwrap_or(0),
            state_setup_us: fields[5].parse().unwrap_or(0),
            pre_execution_us: fields[6].parse().unwrap_or(0),
            tx_execution_us: fields[7].parse().unwrap_or(0),
            post_execution_us: fields[8].parse().unwrap_or(0),
            merge_transitions_us: fields[9].parse().unwrap_or(0),
            hashed_state_us: fields[10].parse().unwrap_or(0),
            triedb_convert_us: fields[11].parse().unwrap_or(0),
            triedb_root_us: fields[12].parse().unwrap_or(0),
            block_assembly_us: fields[13].parse().unwrap_or(0),
            finish_us: fields[14].parse().unwrap_or(0),
            total_us: fields[15].parse().unwrap_or(0),
            hashed_accounts: fields[16].parse().unwrap_or(0),
            hashed_storage_slots: fields[17].parse().unwrap_or(0),
            has_difflayer: fields[18].parse().unwrap_or(false),
            has_cached_reads: fields[19].parse().unwrap_or(false),
        };
        timings.push(t);
    }

    Ok(timings)
}

/// Print summary statistics for a set of timings.
pub fn print_summary(timings: &[BlockTiming], label: &str) {
    if timings.is_empty() {
        println!("No timing data for '{}'", label);
        return;
    }

    println!("\n{}", "=".repeat(60));
    println!("  Summary: {} ({} blocks)", label, timings.len());
    println!("{}\n", "=".repeat(60));

    // Collect phase values
    let phases: Vec<(&str, Vec<u128>)> = vec![
        ("state_setup", timings.iter().map(|t| t.state_setup_us).collect()),
        ("pre_execution", timings.iter().map(|t| t.pre_execution_us).collect()),
        ("tx_execution", timings.iter().map(|t| t.tx_execution_us).collect()),
        ("post_execution", timings.iter().map(|t| t.post_execution_us).collect()),
        ("merge_transitions", timings.iter().map(|t| t.merge_transitions_us).collect()),
        ("hashed_state", timings.iter().map(|t| t.hashed_state_us).collect()),
        ("triedb_convert", timings.iter().map(|t| t.triedb_convert_us).collect()),
        ("triedb_root", timings.iter().map(|t| t.triedb_root_us).collect()),
        ("block_assembly", timings.iter().map(|t| t.block_assembly_us).collect()),
        ("finish (total)", timings.iter().map(|t| t.finish_us).collect()),
        ("TOTAL", timings.iter().map(|t| t.total_us).collect()),
    ];

    println!("{:<20} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Phase", "Mean(us)", "P50(us)", "P90(us)", "P95(us)", "P99(us)");
    println!("{}", "-".repeat(80));

    for (name, values) in &phases {
        let stats = compute_stats(values);
        println!("{:<20} {:>10.0} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
            name, stats.mean, stats.p50, stats.p90, stats.p95, stats.p99);
    }

    // Cold vs Warm comparison
    let cold: Vec<&BlockTiming> = timings.iter().filter(|t| !t.has_cached_reads).collect();
    let warm: Vec<&BlockTiming> = timings.iter().filter(|t| t.has_cached_reads).collect();

    if !cold.is_empty() && !warm.is_empty() {
        println!("\n  Cold vs Warm (finish phase):");
        let cold_finish: Vec<u128> = cold.iter().map(|t| t.finish_us).collect();
        let warm_finish: Vec<u128> = warm.iter().map(|t| t.finish_us).collect();
        let cold_stats = compute_stats(&cold_finish);
        let warm_stats = compute_stats(&warm_finish);
        println!("    Cold ({:>3} blocks): mean={:.0}us, p50={:.0}us",
            cold.len(), cold_stats.mean, cold_stats.p50);
        println!("    Warm ({:>3} blocks): mean={:.0}us, p50={:.0}us",
            warm.len(), warm_stats.mean, warm_stats.p50);
        if warm_stats.mean > 0.0 {
            println!("    Cold/Warm ratio: {:.2}x", cold_stats.mean / warm_stats.mean);
        }
    }

    // Throughput
    let total_time_s = timings.iter().map(|t| t.total_us).sum::<u128>() as f64 / 1_000_000.0;
    let total_gas: u64 = timings.iter().map(|t| t.gas_used).sum();
    let total_txs: usize = timings.iter().map(|t| t.tx_count).sum();

    println!("\n  Throughput:");
    println!("    Blocks/sec:     {:.1}", timings.len() as f64 / total_time_s);
    println!("    TX/sec:         {:.0}", total_txs as f64 / total_time_s);
    println!("    Gas/sec:        {:.0}", total_gas as f64 / total_time_s);
    println!("    Total gas:      {}", total_gas);
    println!("    Total txs:      {}", total_txs);
}

/// Compare two benchmark runs side-by-side.
pub fn compare(baseline_path: &Path, optimized_path: &Path) -> eyre::Result<()> {
    let baseline = read_csv(baseline_path)?;
    let optimized = read_csv(optimized_path)?;

    println!("\n=== A/B Comparison ===\n");

    print_summary(&baseline, "Baseline");
    print_summary(&optimized, "Optimized");

    // Side-by-side comparison
    let phases = [
        "state_setup", "pre_execution", "tx_execution", "post_execution",
        "merge_transitions", "hashed_state", "triedb_convert", "triedb_root",
        "block_assembly", "finish", "TOTAL",
    ];

    let baseline_means = phase_means(&baseline);
    let optimized_means = phase_means(&optimized);

    println!("\n{:<20} {:>12} {:>12} {:>10}",
        "Phase", "Baseline(us)", "Optimized(us)", "Change(%)");
    println!("{}", "-".repeat(60));

    for phase in &phases {
        let b = baseline_means.get(*phase).copied().unwrap_or(0.0);
        let o = optimized_means.get(*phase).copied().unwrap_or(0.0);
        let pct = if b > 0.0 { ((o - b) / b) * 100.0 } else { 0.0 };
        let indicator = if pct < -1.0 { " FASTER" } else if pct > 1.0 { " SLOWER" } else { "" };
        println!("{:<20} {:>12.0} {:>12.0} {:>+9.1}%{}",
            phase, b, o, pct, indicator);
    }

    Ok(())
}

fn phase_means(timings: &[BlockTiming]) -> HashMap<&'static str, f64> {
    let n = timings.len() as f64;
    if n == 0.0 {
        return HashMap::new();
    }
    let mut m = HashMap::new();
    m.insert("state_setup", timings.iter().map(|t| t.state_setup_us as f64).sum::<f64>() / n);
    m.insert("pre_execution", timings.iter().map(|t| t.pre_execution_us as f64).sum::<f64>() / n);
    m.insert("tx_execution", timings.iter().map(|t| t.tx_execution_us as f64).sum::<f64>() / n);
    m.insert("post_execution", timings.iter().map(|t| t.post_execution_us as f64).sum::<f64>() / n);
    m.insert("merge_transitions", timings.iter().map(|t| t.merge_transitions_us as f64).sum::<f64>() / n);
    m.insert("hashed_state", timings.iter().map(|t| t.hashed_state_us as f64).sum::<f64>() / n);
    m.insert("triedb_convert", timings.iter().map(|t| t.triedb_convert_us as f64).sum::<f64>() / n);
    m.insert("triedb_root", timings.iter().map(|t| t.triedb_root_us as f64).sum::<f64>() / n);
    m.insert("block_assembly", timings.iter().map(|t| t.block_assembly_us as f64).sum::<f64>() / n);
    m.insert("finish", timings.iter().map(|t| t.finish_us as f64).sum::<f64>() / n);
    m.insert("TOTAL", timings.iter().map(|t| t.total_us as f64).sum::<f64>() / n);
    m
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
