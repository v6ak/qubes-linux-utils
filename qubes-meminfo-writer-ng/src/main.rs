// qubes-meminfo-writer-ng – Qubes memory information reporter (Rust rewrite).
//
// This program periodically reads system memory information and reports the
// amount of "used" memory (RAM + swap) to xenstore at "memory/meminfo", so
// that the qmemman daemon in dom0 can perform memory ballooning.
//
// Memory is read via the `sysinfo` crate for RAM, and by parsing
// /proc/swaps for per-swap-device/file information.  When running inside a
// Xen domain the RAM total can optionally be overridden by the value reported
// by the Xen balloon driver (current_kb).
//
// Runtime configuration (threshold, delay) is read from a configuration file
// (default: /etc/qubes/meminfo-writer-ng.conf).  The only CLI flags are:
//   --debug            print values to stdout; do not write to xenstore
//   --config <path>    use an alternative configuration file

mod xenstore;

use std::fs;
use std::io;
use std::process;
use std::thread;
use std::time::Duration;

use sysinfo::System;

// Xenstore key written by this program (same as the original C implementation).
const XENSTORE_MEMINFO_PATH: &str = "memory/meminfo";

// Path to the Xen balloon driver's view of current memory in kB.
const XEN_CURRENT_KB_PATH: &str =
    "/sys/devices/system/xen_memory/xen_memory0/info/current_kb";

// Default path to the configuration file.
const DEFAULT_CONFIG_PATH: &str = "/etc/qubes/meminfo-writer-ng.conf";

// Built-in defaults used when a key is absent from the config file.
const DEFAULT_THRESHOLD_KB: u64 = 30_000;
const DEFAULT_DELAY_US: u64 = 100_000;

// ── Swap information ──────────────────────────────────────────────────────────

/// Swap usage for a single device or file as reported by /proc/swaps.
#[derive(Debug)]
struct SwapEntry {
    /// Device path or file name (first column of /proc/swaps).
    filename: String,
    /// Total size in kB.
    total_kb: u64,
    /// Amount in use in kB.
    used_kb: u64,
}

/// Parse /proc/swaps and return one `SwapEntry` per active swap area.
///
/// The kernel file format is:
/// ```text
/// Filename                        Type    Size    Used    Priority
/// /dev/sda2                       partition 8388604 0 -2
/// /swapfile                       file    2097148 1024 -3
/// ```
fn read_swap_entries() -> io::Result<Vec<SwapEntry>> {
    let content = fs::read_to_string("/proc/swaps")?;
    let mut entries = Vec::new();

    // The first line is the header; skip it.
    for line in content.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let filename = match fields.next() {
            Some(f) => f.to_string(),
            None => continue,
        };
        let _swap_type = fields.next(); // "partition" or "file" – ignored
        let total_kb: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let used_kb: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        entries.push(SwapEntry {
            filename,
            total_kb,
            used_kb,
        });
    }

    Ok(entries)
}

// ── Xen helpers ───────────────────────────────────────────────────────────────

/// Read the Xen balloon driver's current memory allocation in kB.
///
/// Returns `None` when the path does not exist (non-Xen machine), is
/// unreadable, or contains zero (meaning "not set").
fn read_xen_current_kb() -> Option<u64> {
    fs::read_to_string(XEN_CURRENT_KB_PATH)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v != 0)
}

// ── Memory calculation ────────────────────────────────────────────────────────

/// Return the total "used" memory in kB: used RAM + used swap.
///
/// When `xen_current_kb` is provided it replaces the kernel-reported total
/// memory (the Xen balloon driver may have shrunk the VM's allocation below
/// what the OS thinks is installed).
fn compute_used_memory(
    total_memory_kb: u64,
    available_memory_kb: u64,
    xen_current_kb: Option<u64>,
    swap_entries: &[SwapEntry],
) -> u64 {
    let effective_total_kb = xen_current_kb.unwrap_or(total_memory_kb);

    // "used RAM" = effective total − memory available (MemAvailable on Linux,
    // which accounts for free pages, buffers and reclaimable cache).
    let used_ram_kb = effective_total_kb.saturating_sub(available_memory_kb);

    let used_swap_kb: u64 = swap_entries.iter().map(|e| e.used_kb).sum();

    used_ram_kb + used_swap_kb
}

/// Decide whether the new value is worth sending to xenstore.
///
/// Mirrors the heuristic of the original C implementation:
/// * Always send on the first update (`prev == 0`).
/// * Send when the absolute change exceeds `threshold_kb`.
/// * Send when memory is under pressure (> ~77 % of total) and the change
///   exceeds half the threshold.
fn should_update(
    used_mem_kb: u64,
    prev_used_mem_kb: u64,
    threshold_kb: u64,
    total_mem_kb: u64,
) -> bool {
    if prev_used_mem_kb == 0 {
        return true;
    }
    let diff = used_mem_kb.abs_diff(prev_used_mem_kb);
    if diff > threshold_kb {
        return true;
    }
    // Pressure heuristic (mirrors the original C implementation):
    // send an early update when memory usage is rising and is already above
    // roughly 77 % of total (10/13 ≈ 0.769).  The +12 in the divisor biases
    // the integer division toward the same boundary as the floating-point
    // equivalent.
    if used_mem_kb > prev_used_mem_kb
        && used_mem_kb * 13 > (total_mem_kb + 12) * 10
        && diff > threshold_kb / 2
    {
        return true;
    }
    false
}

// ── Configuration file ────────────────────────────────────────────────────────

/// Settings loaded from the configuration file.
struct Config {
    /// Minimum memory change (in kB) that triggers a xenstore write.
    threshold_kb: u64,
    /// Sleep interval between updates in microseconds.
    delay_us: u64,
}

/// Parse the configuration file at `path`.
///
/// The file format is simple `KEY=VALUE` pairs, one per line.  Lines starting
/// with `#` (after optional leading whitespace) and blank lines are ignored.
/// Unknown keys are also ignored so that future versions can add new settings
/// without breaking older binaries.
///
/// Recognised keys:
///   THRESHOLD   – memory change threshold in kB (positive integer)
///   DELAY       – update interval in microseconds (positive integer)
fn parse_config(path: &str) -> Config {
    let mut threshold_kb = DEFAULT_THRESHOLD_KB;
    let mut delay_us = DEFAULT_DELAY_US;

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not read config file {path}: {e}; using defaults");
            return Config { threshold_kb, delay_us };
        }
    };

    for (line_index, line) in content.lines().enumerate() {
        let line = line.trim();
        // Skip blank lines and comments.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            eprintln!("warning: {path}:{}: malformed line (expected KEY=VALUE)", line_index + 1);
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match key {
            "THRESHOLD" => match value.parse::<u64>() {
                Ok(v) if v > 0 => threshold_kb = v,
                _ => eprintln!(
                    "warning: {path}:{}: THRESHOLD must be a positive integer; using default",
                    line_index + 1
                ),
            },
            "DELAY" => match value.parse::<u64>() {
                Ok(v) if v > 0 => delay_us = v,
                _ => eprintln!(
                    "warning: {path}:{}: DELAY must be a positive integer; using default",
                    line_index + 1
                ),
            },
            // Silently ignore unknown keys for forward compatibility.
            _ => {}
        }
    }

    Config { threshold_kb, delay_us }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

struct CliArgs {
    /// Path to the configuration file.
    config_path: String,
    /// When true, print computed values to stdout instead of writing xenstore.
    debug: bool,
}

fn print_usage(prog: &str) {
    eprintln!("Usage: {prog} [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --config <path>    Configuration file [default: {DEFAULT_CONFIG_PATH}]");
    eprintln!("  --debug            Print values to stdout; do not write to xenstore");
    eprintln!("  --help, -h         Show this message");
    eprintln!();
    eprintln!("Configuration file keys (KEY=VALUE format):");
    eprintln!("  THRESHOLD          Memory change threshold in kB [default: {DEFAULT_THRESHOLD_KB}]");
    eprintln!("  DELAY              Update interval in microseconds [default: {DEFAULT_DELAY_US}]");
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("qubes-meminfo-writer-ng");

    let mut config_path = DEFAULT_CONFIG_PATH.to_string();
    let mut debug = false;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                match args.get(i) {
                    Some(p) => config_path = p.clone(),
                    None => {
                        eprintln!("error: --config requires a path argument");
                        print_usage(prog);
                        process::exit(1);
                    }
                }
            }
            "--debug" => debug = true,
            "--help" | "-h" => {
                print_usage(prog);
                process::exit(0);
            }
            other => {
                eprintln!("error: unknown option: {other}");
                print_usage(prog);
                process::exit(1);
            }
        }
        i += 1;
    }

    CliArgs { config_path, debug }
}

// ── Main loop ─────────────────────────────────────────────────────────────────

fn main() {
    let cli = parse_args();
    let cfg = parse_config(&cli.config_path);

    // Open xenstore unless we are in debug mode.
    let xs = if cli.debug {
        None
    } else {
        match xenstore::XsHandle::open() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("error: failed to open xenstore: {e}");
                process::exit(1);
            }
        }
    };

    let mut sys = System::new();
    let mut prev_used_mem_kb: u64 = 0;

    loop {
        sys.refresh_memory();

        let total_memory_kb = sys.total_memory() / 1024;
        let available_memory_kb = sys.available_memory() / 1024;
        let xen_current_kb = read_xen_current_kb();

        // Read per-swap-device/file information from /proc/swaps.
        let swap_entries = match read_swap_entries() {
            Ok(e) => e,
            Err(err) => {
                eprintln!("warning: could not read /proc/swaps: {err}");
                Vec::new()
            }
        };

        let swap_total_kb: u64 = swap_entries.iter().map(|e| e.total_kb).sum();
        let swap_used_kb: u64 = swap_entries.iter().map(|e| e.used_kb).sum();

        let used_mem_kb =
            compute_used_memory(total_memory_kb, available_memory_kb, xen_current_kb, &swap_entries);

        if cli.debug {
            println!(
                "total_mem={total_memory_kb}kB \
                 available_mem={available_memory_kb}kB \
                 swap_total={swap_total_kb}kB \
                 swap_used={swap_used_kb}kB \
                 xen_current={}kB \
                 used_mem={used_mem_kb}kB",
                xen_current_kb.unwrap_or(0)
            );
            for entry in &swap_entries {
                println!(
                    "  swap: {} total={}kB used={}kB",
                    entry.filename, entry.total_kb, entry.used_kb
                );
            }
        }

        if should_update(used_mem_kb, prev_used_mem_kb, cfg.threshold_kb, total_memory_kb) {
            prev_used_mem_kb = used_mem_kb;
            let data = used_mem_kb.to_string();

            if cli.debug {
                println!("xenstore write: {XENSTORE_MEMINFO_PATH}={data}");
            } else if let Some(ref h) = xs {
                if let Err(e) = h.write(XENSTORE_MEMINFO_PATH, &data) {
                    eprintln!("error: xenstore write failed: {e}");
                    process::exit(1);
                }
            }
        }

        thread::sleep(Duration::from_micros(cfg.delay_us));
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal RAII temp-file helper so tests do not depend on the `tempfile`
    // crate (which may not be packaged by the distribution).
    struct TempFile {
        path: std::path::PathBuf,
    }
    impl TempFile {
        fn new(content: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "qubes-meminfo-test-{}-{}.conf",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::write(&path, content).expect("write temp config");
            TempFile { path }
        }
        fn path_str(&self) -> &str {
            self.path.to_str().expect("temp path is valid UTF-8")
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn make_swap(total_kb: u64, used_kb: u64) -> SwapEntry {
        SwapEntry {
            filename: "/dev/test".into(),
            total_kb,
            used_kb,
        }
    }

    #[test]
    fn test_compute_used_memory_no_swap_no_xen() {
        let total = 8_000_000;
        let available = 6_000_000;
        let result = compute_used_memory(total, available, None, &[]);
        assert_eq!(result, 2_000_000);
    }

    #[test]
    fn test_compute_used_memory_with_swap() {
        let entries = vec![
            make_swap(2_097_148, 1_024),
            make_swap(1_048_576, 512),
        ];
        let result = compute_used_memory(8_000_000, 6_000_000, None, &entries);
        assert_eq!(result, 2_000_000 + 1_024 + 512);
    }

    #[test]
    fn test_compute_used_memory_xen_override() {
        // Xen reports a smaller allocation than the kernel thinks is installed.
        let result = compute_used_memory(8_000_000, 6_000_000, Some(4_000_000), &[]);
        // effective_total=4_000_000, used_ram = 4_000_000 − 6_000_000 → saturates to 0
        // (Xen allocation is smaller than reported MemAvailable; no used RAM.)
        assert_eq!(result, 0);
    }

    #[test]
    fn test_should_update_first_call() {
        assert!(should_update(1_000_000, 0, 30_000, 8_000_000));
    }

    #[test]
    fn test_should_update_large_change() {
        assert!(should_update(1_100_000, 1_000_000, 30_000, 8_000_000));
    }

    #[test]
    fn test_should_update_small_change_no_pressure() {
        // Change of 10 000 kB < threshold of 30 000 kB, and not under pressure.
        assert!(!should_update(1_010_000, 1_000_000, 30_000, 8_000_000));
    }

    #[test]
    fn test_should_update_pressure_heuristic() {
        // Memory at 80% of total (above the ~77% threshold), change > half threshold.
        let total = 8_000_000u64;
        let used = total * 80 / 100;
        let prev = used - 20_000; // change of 20 000 > threshold/2=15 000
        assert!(should_update(used, prev, 30_000, total));
    }

    #[test]
    fn test_parse_proc_swaps() {
        // The test only verifies the logic against a real /proc/swaps if it
        // exists; it does not fail on machines without swap.
        if let Ok(entries) = read_swap_entries() {
            for e in &entries {
                assert!(!e.filename.is_empty());
                assert!(e.used_kb <= e.total_kb, "used ≤ total for {}", e.filename);
            }
        }
    }

    #[test]
    fn test_parse_config_defaults() {
        // With a non-existent file the defaults are returned without panic.
        let cfg = parse_config("/nonexistent/path/config.conf");
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
    }

    #[test]
    fn test_parse_config_values() {
        let tmp = TempFile::new("# comment\nTHRESHOLD=50000\nDELAY=200000\nUNKNOWN=ignored\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, 50_000);
        assert_eq!(cfg.delay_us, 200_000);
    }

    #[test]
    fn test_parse_config_bad_values_use_defaults() {
        let tmp = TempFile::new("THRESHOLD=notanumber\nDELAY=0\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
    }
}
