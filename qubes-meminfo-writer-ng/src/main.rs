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
// Runtime configuration is read from a TOML file
// (default: /etc/qubes/meminfo-writer-ng.conf).  CLI flags:
//   --output <mode>    print | xenstore | both  (default: xenstore)
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

// ── Glob matching ─────────────────────────────────────────────────────────────

/// Match a shell-style glob pattern against a string.
///
/// Only `*` (matches any sequence of characters, including none) and `?`
/// (matches exactly one character) are supported.  Character classes (`[…]`),
/// brace expansion, and escape sequences are **not** interpreted.  No
/// directory-separator special-casing is applied.
fn glob_matches(pattern: &str, name: &str) -> bool {
    glob_match_impl(pattern.as_bytes(), name.as_bytes())
}

fn glob_match_impl(pat: &[u8], s: &[u8]) -> bool {
    match (pat.split_first(), s.split_first()) {
        (None, None) => true,
        (Some((&b'*', rest_pat)), _) => {
            // '*' matches zero characters of `s` …
            glob_match_impl(rest_pat, s)
                // … or one more character of `s`.
                || s.split_first().is_some_and(|(_, rest_s)| glob_match_impl(pat, rest_s))
        }
        (Some((&b'?', rest_pat)), Some((_, rest_s))) => glob_match_impl(rest_pat, rest_s),
        (Some((&p, rest_pat)), Some((&c, rest_s))) if p == c => {
            glob_match_impl(rest_pat, rest_s)
        }
        _ => false,
    }
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

// ── Swap weights ──────────────────────────────────────────────────────────────

/// A swap-weight rule: the first matching rule for a device/file is applied.
#[derive(Debug, Clone)]
struct SwapWeightRule {
    /// Shell glob pattern matched against the swap device/file path.
    glob: String,
    /// Multiplier applied to the used-kB value.
    /// `0.0` = ignore this swap entirely, `1.0` = count in full.
    weight: f64,
}

/// Look up the weight for a swap entry using the configured rules.
/// Returns `1.0` if no rule matches (full weight by default).
fn swap_weight(filename: &str, rules: &[SwapWeightRule]) -> f64 {
    rules
        .iter()
        .find(|r| glob_matches(&r.glob, filename))
        .map(|r| r.weight)
        .unwrap_or(1.0)
}

// ── Memory calculation ────────────────────────────────────────────────────────

/// Return the total "used" memory in kB: used RAM + weighted used swap.
///
/// When `xen_current_kb` is provided it replaces the kernel-reported total
/// memory (the Xen balloon driver may have shrunk the VM's allocation below
/// what the OS thinks is installed).
///
/// Each swap entry's used-kB is multiplied by the weight of the first
/// matching `swap_weight_rules` entry (default 1.0).
fn compute_used_memory(
    total_memory_kb: u64,
    available_memory_kb: u64,
    xen_current_kb: Option<u64>,
    swap_entries: &[SwapEntry],
    swap_weight_rules: &[SwapWeightRule],
) -> u64 {
    let effective_total_kb = xen_current_kb.unwrap_or(total_memory_kb);

    // "used RAM" = effective total − memory available (MemAvailable on Linux,
    // which accounts for free pages, buffers and reclaimable cache).
    let used_ram_kb = effective_total_kb.saturating_sub(available_memory_kb);

    let used_swap_kb: u64 = swap_entries
        .iter()
        .map(|e| {
            let w = swap_weight(&e.filename, swap_weight_rules);
            (e.used_kb as f64 * w) as u64
        })
        .sum();

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

// ── Configuration file (TOML) ─────────────────────────────────────────────────

/// Settings loaded from the TOML configuration file.
struct Config {
    /// Minimum memory change (in kB) that triggers a xenstore write.
    threshold_kb: u64,
    /// Sleep interval between updates in microseconds.
    delay_us: u64,
    /// Ordered list of swap-weight rules (first match wins).
    swap_weights: Vec<SwapWeightRule>,
}

/// Finish a pending `[[swap]]` section and push it onto `weights`.
///
/// Called whenever a new `[[swap]]` header is encountered and at EOF.
/// Emits a warning if either `glob` or `weight` is missing.
fn flush_swap_rule(
    weights: &mut Vec<SwapWeightRule>,
    glob: &mut Option<String>,
    weight: &mut Option<f64>,
    path: &str,
    line_no: usize,
) {
    match (glob.take(), weight.take()) {
        (Some(g), Some(w)) => weights.push(SwapWeightRule { glob: g, weight: w }),
        (None, _) => eprintln!("warning: {path}:{line_no}: [[swap]] section missing 'glob'"),
        (Some(_), None) => eprintln!("warning: {path}:{line_no}: [[swap]] section missing 'weight'"),
    }
}

/// Parse a TOML basic-string literal (surrounding `"…"` quotes), returning
/// the inner content.  Escape sequences are not interpreted; the raw bytes
/// between the quotes are returned as-is.
fn parse_toml_string(s: &str, path: &str, line_no: usize) -> Option<String> {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        Some(s[1..s.len() - 1].to_string())
    } else {
        eprintln!("warning: {path}:{line_no}: expected a quoted string value");
        None
    }
}

/// Parse the TOML configuration file at `path`.
///
/// Recognised top-level keys:
///   `threshold_kb`   – memory change threshold in kB (positive integer)
///   `delay_us`       – update interval in microseconds (positive integer)
///
/// Recognised array-of-tables sections:
///   `[[swap]]`       – swap weight rule with keys `glob` (string) and
///                      `weight` (non-negative float).  The first matching
///                      rule for each swap device is applied.
fn parse_config(path: &str) -> Config {
    let mut threshold_kb = DEFAULT_THRESHOLD_KB;
    let mut delay_us = DEFAULT_DELAY_US;
    let mut swap_weights: Vec<SwapWeightRule> = Vec::new();

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not read config file {path}: {e}; using defaults");
            return Config { threshold_kb, delay_us, swap_weights };
        }
    };

    #[derive(PartialEq)]
    enum Section {
        TopLevel,
        Swap,
    }

    let mut section = Section::TopLevel;
    let mut cur_glob: Option<String> = None;
    let mut cur_weight: Option<f64> = None;
    let total_lines = content.lines().count();

    for (line_index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Array-of-tables header: [[section]]
        if line.starts_with("[[") && line.ends_with("]]") {
            if section == Section::Swap {
                flush_swap_rule(
                    &mut swap_weights,
                    &mut cur_glob,
                    &mut cur_weight,
                    path,
                    line_index + 1,
                );
            }
            let name = line[2..line.len() - 2].trim();
            match name {
                "swap" => section = Section::Swap,
                other => {
                    eprintln!(
                        "warning: {path}:{}: unknown section [[{other}]]; skipping",
                        line_index + 1
                    );
                    section = Section::TopLevel;
                }
            }
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            eprintln!(
                "warning: {path}:{}: malformed line (expected key = value)",
                line_index + 1
            );
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match section {
            Section::TopLevel => match key {
                "threshold_kb" => match value.parse::<u64>() {
                    Ok(v) if v > 0 => threshold_kb = v,
                    _ => eprintln!(
                        "warning: {path}:{}: threshold_kb must be a positive integer; using default",
                        line_index + 1
                    ),
                },
                "delay_us" => match value.parse::<u64>() {
                    Ok(v) if v > 0 => delay_us = v,
                    _ => eprintln!(
                        "warning: {path}:{}: delay_us must be a positive integer; using default",
                        line_index + 1
                    ),
                },
                _ => {}
            },
            Section::Swap => match key {
                "glob" => cur_glob = parse_toml_string(value, path, line_index + 1),
                "weight" => match value.parse::<f64>() {
                    Ok(w) if w.is_finite() && w >= 0.0 => cur_weight = Some(w),
                    _ => eprintln!(
                        "warning: {path}:{}: weight must be a non-negative number; using default",
                        line_index + 1
                    ),
                },
                _ => {}
            },
        }
    }

    // Finish the final [[swap]] section (if any).
    if section == Section::Swap {
        flush_swap_rule(
            &mut swap_weights,
            &mut cur_glob,
            &mut cur_weight,
            path,
            total_lines + 1,
        );
    }

    Config { threshold_kb, delay_us, swap_weights }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

/// Output mode: controls where computed values and xenstore writes go.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OutputMode {
    /// Print computed values to stdout only; do not write to xenstore (a).
    Print,
    /// Write to xenstore only; no stdout output (b, default).
    Xenstore,
    /// Write to xenstore AND print computed values to stdout (c).
    Both,
}

struct CliArgs {
    /// Path to the configuration file.
    config_path: String,
    /// Selected output mode.
    output_mode: OutputMode,
}

fn print_usage(prog: &str) {
    eprintln!("Usage: {prog} [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --config <path>        Configuration file [default: {DEFAULT_CONFIG_PATH}]");
    eprintln!("  --output <mode>        Output mode (default: xenstore):");
    eprintln!("    print                Print computed values to stdout; no xenstore writes");
    eprintln!("    xenstore             Write to xenstore only (silent)");
    eprintln!("    both                 Write to xenstore and print to stdout");
    eprintln!("  --help, -h             Show this message");
    eprintln!();
    eprintln!("Configuration file (TOML) keys:");
    eprintln!("  threshold_kb   Memory change threshold in kB [default: {DEFAULT_THRESHOLD_KB}]");
    eprintln!("  delay_us       Update interval in microseconds [default: {DEFAULT_DELAY_US}]");
    eprintln!();
    eprintln!("  [[swap]]       Swap weight rule (first matching rule applies):");
    eprintln!("    glob         Shell glob pattern for the swap device/file path");
    eprintln!("    weight       Multiplier for used-kB (0.0 = ignore, 1.0 = full weight)");
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("qubes-meminfo-writer-ng");

    let mut config_path = DEFAULT_CONFIG_PATH.to_string();
    let mut output_mode = OutputMode::Xenstore;

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
            "--output" => {
                i += 1;
                match args.get(i).map(String::as_str) {
                    Some("print") => output_mode = OutputMode::Print,
                    Some("xenstore") => output_mode = OutputMode::Xenstore,
                    Some("both") => output_mode = OutputMode::Both,
                    Some(other) => {
                        eprintln!("error: unknown --output mode '{other}'; expected print|xenstore|both");
                        print_usage(prog);
                        process::exit(1);
                    }
                    None => {
                        eprintln!("error: --output requires a mode argument");
                        print_usage(prog);
                        process::exit(1);
                    }
                }
            }
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

    CliArgs { config_path, output_mode }
}

// ── Main loop ─────────────────────────────────────────────────────────────────

fn main() {
    let cli = parse_args();
    let cfg = parse_config(&cli.config_path);

    let use_xenstore = matches!(cli.output_mode, OutputMode::Xenstore | OutputMode::Both);
    let use_print = matches!(cli.output_mode, OutputMode::Print | OutputMode::Both);

    // Open xenstore unless we are in print-only mode.
    let xs = if use_xenstore {
        match xenstore::XsHandle::open() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("error: failed to open xenstore: {e}");
                process::exit(1);
            }
        }
    } else {
        None
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
        let swap_used_kb_raw: u64 = swap_entries.iter().map(|e| e.used_kb).sum();

        let used_mem_kb = compute_used_memory(
            total_memory_kb,
            available_memory_kb,
            xen_current_kb,
            &swap_entries,
            &cfg.swap_weights,
        );

        if use_print {
            println!(
                "total_mem={total_memory_kb}kB \
                 available_mem={available_memory_kb}kB \
                 swap_total={swap_total_kb}kB \
                 swap_used_kb_raw={swap_used_kb_raw}kB \
                 xen_current={}kB \
                 used_mem={used_mem_kb}kB",
                xen_current_kb.unwrap_or(0)
            );
            for entry in &swap_entries {
                let w = swap_weight(&entry.filename, &cfg.swap_weights);
                println!(
                    "  swap: {} total={}kB used_kb_raw={}kB weight={w:.3} used_kb_weighted={}kB",
                    entry.filename,
                    entry.total_kb,
                    entry.used_kb,
                    (entry.used_kb as f64 * w) as u64,
                );
            }
        }

        let data = used_mem_kb.to_string();
        let will_write = should_update(used_mem_kb, prev_used_mem_kb, cfg.threshold_kb, total_memory_kb);
        if use_print {
            if will_write {
                println!("xenstore write: {XENSTORE_MEMINFO_PATH}={data}");
            } else {
                println!("xenstore skipped (threshold): {XENSTORE_MEMINFO_PATH}={data}");
            }
        }
        if will_write {
            prev_used_mem_kb = used_mem_kb;
            if let Some(ref h) = xs {
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
                "qubes-meminfo-test-{}-{}.toml",
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

    fn make_swap(filename: &str, total_kb: u64, used_kb: u64) -> SwapEntry {
        SwapEntry { filename: filename.into(), total_kb, used_kb }
    }

    fn no_weights() -> Vec<SwapWeightRule> {
        vec![]
    }

    // ── glob_matches ─────────────────────────────────────────────────────────

    #[test]
    fn test_glob_exact_match() {
        assert!(glob_matches("/dev/sda2", "/dev/sda2"));
        assert!(!glob_matches("/dev/sda2", "/dev/sda3"));
    }

    #[test]
    fn test_glob_star_wildcard() {
        assert!(glob_matches("/dev/zram*", "/dev/zram0"));
        assert!(glob_matches("/dev/zram*", "/dev/zram42"));
        assert!(glob_matches("*", "/dev/sda2"));
        assert!(glob_matches("*", ""));
        assert!(!glob_matches("/dev/zram*", "/dev/sda2"));
    }

    #[test]
    fn test_glob_question_wildcard() {
        assert!(glob_matches("/dev/sd?2", "/dev/sda2"));
        assert!(glob_matches("/dev/sd?2", "/dev/sdb2"));
        assert!(!glob_matches("/dev/sd?2", "/dev/sda3"));
    }

    // ── swap_weight ───────────────────────────────────────────────────────────

    #[test]
    fn test_swap_weight_no_rules_defaults_to_one() {
        assert_eq!(swap_weight("/dev/sda2", &[]), 1.0);
    }

    #[test]
    fn test_swap_weight_first_match_wins() {
        let rules = vec![
            SwapWeightRule { glob: "/dev/zram*".into(), weight: 0.5 },
            SwapWeightRule { glob: "*".into(), weight: 1.0 },
        ];
        assert_eq!(swap_weight("/dev/zram0", &rules), 0.5);
        assert_eq!(swap_weight("/dev/sda2", &rules), 1.0);
    }

    // ── compute_used_memory ───────────────────────────────────────────────────

    #[test]
    fn test_compute_used_memory_no_swap_no_xen() {
        let result = compute_used_memory(8_000_000, 6_000_000, None, &[], &no_weights());
        assert_eq!(result, 2_000_000);
    }

    #[test]
    fn test_compute_used_memory_with_swap_full_weight() {
        let entries = vec![
            make_swap("/dev/sda2", 2_097_148, 1_024),
            make_swap("/swapfile", 1_048_576, 512),
        ];
        let result = compute_used_memory(8_000_000, 6_000_000, None, &entries, &no_weights());
        assert_eq!(result, 2_000_000 + 1_024 + 512);
    }

    #[test]
    fn test_compute_used_memory_with_swap_half_weight() {
        let entries = vec![make_swap("/dev/zram0", 1_000_000, 1_000)];
        let rules = vec![SwapWeightRule { glob: "/dev/zram*".into(), weight: 0.5 }];
        let result = compute_used_memory(8_000_000, 6_000_000, None, &entries, &rules);
        // used_ram=2_000_000, used_swap=500 (1000*0.5)
        assert_eq!(result, 2_000_000 + 500);
    }

    #[test]
    fn test_compute_used_memory_xen_override() {
        // Xen reports a smaller allocation than the kernel thinks is installed.
        let result = compute_used_memory(8_000_000, 6_000_000, Some(4_000_000), &[], &no_weights());
        // effective_total=4_000_000, used_ram = 4_000_000 − 6_000_000 → saturates to 0
        assert_eq!(result, 0);
    }

    // ── should_update ─────────────────────────────────────────────────────────

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
        assert!(!should_update(1_010_000, 1_000_000, 30_000, 8_000_000));
    }

    #[test]
    fn test_should_update_pressure_heuristic() {
        let total = 8_000_000u64;
        let used = total * 80 / 100;
        let prev = used - 20_000;
        assert!(should_update(used, prev, 30_000, total));
    }

    // ── parse_config ──────────────────────────────────────────────────────────

    #[test]
    fn test_parse_config_defaults() {
        let cfg = parse_config("/nonexistent/path/config.toml");
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
        assert!(cfg.swap_weights.is_empty());
    }

    #[test]
    fn test_parse_config_scalar_values() {
        let tmp = TempFile::new(
            "# comment\nthreshold_kb = 50000\ndelay_us = 200000\n",
        );
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, 50_000);
        assert_eq!(cfg.delay_us, 200_000);
    }

    #[test]
    fn test_parse_config_swap_weights() {
        let tmp = TempFile::new(
            "threshold_kb = 30000\n\
             [[swap]]\nglob = \"/dev/zram*\"\nweight = 0.5\n\
             [[swap]]\nglob = \"*\"\nweight = 1.0\n",
        );
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.swap_weights.len(), 2);
        assert_eq!(cfg.swap_weights[0].glob, "/dev/zram*");
        assert_eq!(cfg.swap_weights[0].weight, 0.5);
        assert_eq!(cfg.swap_weights[1].glob, "*");
        assert_eq!(cfg.swap_weights[1].weight, 1.0);
    }

    #[test]
    fn test_parse_config_bad_values_use_defaults() {
        let tmp = TempFile::new("threshold_kb = notanumber\ndelay_us = 0\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
    }

    // ── read_swap_entries ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_proc_swaps() {
        if let Ok(entries) = read_swap_entries() {
            for e in &entries {
                assert!(!e.filename.is_empty());
                assert!(e.used_kb <= e.total_kb, "used ≤ total for {}", e.filename);
            }
        }
    }
}
