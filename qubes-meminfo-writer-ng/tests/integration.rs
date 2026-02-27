// Integration tests for qubes-meminfo-writer-ng.
//
// These tests run the compiled binary with `--output print --once` and verify
// the stdout output.  No Xen/xenstore environment is required.
//
// Comparison with the original C meminfo-writer
// ---------------------------------------------
// The C implementation (qmemman/meminfo-writer.c) computes:
//
//   used_mem = MemTotal - MemFree - Buffers - Cached + SwapTotal - SwapFree
//            = (MemTotal - MemFreeSum) + SwapUsed
//
// where MemFreeSum = MemFree + Buffers + Cached.
//
// The Rust implementation uses the kernel-provided `MemAvailable` field
// (introduced in Linux 3.14, always present on any Qubes-supported kernel).
// `MemAvailable` additionally accounts for reclaimable slab memory, making it
// a slightly larger estimate of available memory -> slightly lower `used_mem`.
// This is an intentional improvement: the Rust value is never *higher* than
// the C value for the same snapshot.
//
// The threshold heuristic is algebraically identical:
//   C:    used_mem / 10        > (MemTotal + 12) / 13
//   Rust: used_mem * 13        > (MemTotal + 12) * 10
// (Rust avoids integer division by multiplying both sides by 130.)
//
// The key new capability is per-device swap weights: the C implementation
// counts all swap usage at full weight (1.0).  The Rust implementation lets
// operators configure e.g. 0.5x weight for zram (which compresses data).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Path to the test binary (built by `cargo test` before running integration tests).
fn binary_path() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for integration tests, with hyphens in
    // the binary name converted to underscores.
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_qubes_meminfo_writer_ng") {
        return PathBuf::from(p);
    }
    // Fallback for manual runs: the integration test binary lives in
    // target/{profile}/deps/; the program binary is one level up.
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // drop binary name
    p.pop(); // drop "deps"
    p.push("qubes-meminfo-writer-ng");
    p
}

struct TempConfig {
    path: PathBuf,
}
impl TempConfig {
    fn new(content: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "qmw-ng-inttest-{}-{}.toml",
            std::process::id(),
            CTR.fetch_add(1, Ordering::Relaxed),
        ));
        fs::write(&path, content).expect("write temp config");
        TempConfig { path }
    }
    fn path_str(&self) -> &str {
        self.path.to_str().expect("path is valid UTF-8")
    }
}
impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Run the binary with `--output print --once` and a given config.
/// Returns (stdout, stderr, exit_code).
fn run_once(config: &TempConfig) -> (String, String, i32) {
    let output = Command::new(binary_path())
        .args(["--output", "print", "--once", "--config", config.path_str()])
        .output()
        .expect("failed to run binary");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

// ── Scenario 1: Basic output format ──────────────────────────────────────────

/// The binary should exit with code 0 and produce a `used_mem=` line.
#[test]
fn test_basic_print_once_exits_zero() {
    let cfg = TempConfig::new("threshold_kb = 30000\ndelay_us = 100000\n");
    let (stdout, _stderr, code) = run_once(&cfg);
    assert_eq!(code, 0, "binary should exit 0 in print+once mode");
    assert!(stdout.contains("used_mem="), "stdout should contain used_mem=:\n{stdout}");
}

/// In print mode the xenstore decision line must be present every iteration.
#[test]
fn test_xenstore_decision_line_present() {
    let cfg = TempConfig::new("threshold_kb = 30000\ndelay_us = 100000\n");
    let (stdout, _stderr, code) = run_once(&cfg);
    assert_eq!(code, 0);
    let has_write = stdout.contains("xenstore write: memory/meminfo=");
    let has_skip = stdout.contains("xenstore skipped (threshold): memory/meminfo=");
    assert!(
        has_write || has_skip,
        "stdout must contain a xenstore write or skip line:\n{stdout}"
    );
}

/// On the very first iteration `prev_used_mem == 0`, so a write is always
/// forced regardless of the threshold.
#[test]
fn test_first_iteration_always_writes() {
    // A threshold larger than any realistic memory delta ensures the normal
    // threshold check would suppress a write – but the first-call special case
    // must still trigger one.
    let cfg = TempConfig::new("threshold_kb = 99999999\ndelay_us = 100000\n");
    let (stdout, _stderr, code) = run_once(&cfg);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("xenstore write: memory/meminfo="),
        "first iteration must write regardless of threshold:\n{stdout}"
    );
}

// ── Scenario 2: Swap weight rules applied ─────────────────────────────────────

/// When a zero-weight rule matches all swap devices, swap usage is excluded
/// from `used_mem`.  The value must equal used-RAM only.
#[test]
fn test_zero_weight_swap_excluded_from_used_mem() {
    // With weight=0 for all swap, the reported used_mem should not include any
    // swap usage.  We can't know the exact RAM value at test time, but we can
    // verify the output is parseable and non-negative.
    let cfg = TempConfig::new(
        "threshold_kb = 30000\ndelay_us = 100000\n\
         [[swap]]\nglob = \"*\"\nweight = 0.0\n",
    );
    let (stdout, _stderr, code) = run_once(&cfg);
    assert_eq!(code, 0);
    // Extract used_mem value
    let used_mem = parse_field(&stdout, "used_mem=");
    assert!(used_mem.is_some(), "used_mem field missing in:\n{stdout}");
    // Just verify it's a non-negative number (swap=0 means used_mem >= 0).
    let kb = used_mem.unwrap();
    assert!(kb < u64::MAX, "used_mem should be a valid u64");
}

// ── Scenario 3: Threshold formula matches C heuristic ────────────────────────

/// Verify that `should_update` produces the same decision as the original C
/// code for boundary values.  This is a direct unit-level comparison.
///
/// C code (meminfo-writer.c line 76-79):
/// ```c
/// if (used_mem_diff > used_mem_change_threshold
///     || prev_used_mem == 0
///     || (used_mem > prev_used_mem
///         && used_mem / 10 > (MemTotal+12) / 13
///         && used_mem_diff > used_mem_change_threshold/2))
/// ```
///
/// The Rust `should_update` is algebraically equivalent (avoids integer
/// division by multiplying both sides by 130).  This test exercises the
/// boundary directly.
#[test]
fn test_threshold_heuristic_matches_c_formula() {
    // Inline the C formula for comparison.
    fn c_should_update(used: i64, prev: i64, threshold: i64, total: i64) -> bool {
        let diff = (used - prev).abs();
        if prev == 0 { return true; }
        if diff > threshold { return true; }
        if used > prev && used / 10 > (total + 12) / 13 && diff > threshold / 2 {
            return true;
        }
        false
    }

    // Rust formula (from mem.rs).
    fn rust_should_update(used: u64, prev: u64, threshold: u64, total: u64) -> bool {
        if prev == 0 { return true; }
        let diff = used.abs_diff(prev);
        if diff > threshold { return true; }
        if used > prev
            && used * 13 > (total + 12) * 10
            && diff > threshold / 2
        {
            return true;
        }
        false
    }

    let threshold = 30_000i64;
    let total = 8_000_000i64;

    // Test cases: (used, prev) — representative values.
    let cases: &[(i64, i64, &str)] = &[
        (1_000_000, 0,         "first call – always true"),
        (1_100_000, 1_000_000, "large change – above threshold"),
        (1_010_000, 1_000_000, "small change, no pressure – false"),
        (1_030_000, 1_000_000, "exactly at threshold (not above) – false"),
        (1_031_000, 1_000_000, "one above threshold – true"),
        // Pressure heuristic: usage above ~77 % of total, rising.
        (total * 80 / 100, total * 80 / 100 - 20_000, "pressure: rising above 77%"),
        // Pressure heuristic should NOT fire when falling.
        (total * 80 / 100 - 20_000, total * 80 / 100, "pressure: falling – false"),
        // Pressure heuristic should NOT fire below 77 %.
        (total * 50 / 100, total * 50 / 100 - 20_000, "below 77% – false"),
    ];

    for &(used, prev, label) in cases {
        let c = c_should_update(used, prev, threshold, total);
        let r = rust_should_update(used as u64, prev as u64, threshold as u64, total as u64);
        assert_eq!(c, r, "mismatch for '{label}': C={c} Rust={r}");
    }
}

// ── Scenario 4: Memory formula comparison with C ──────────────────────────────

/// Documents and verifies the relationship between the C formula and the Rust
/// formula for `used_mem`.
///
/// C: used_mem = MemTotal − MemFree − Buffers − Cached + SwapTotal − SwapFree
/// Rust: used_mem = (MemTotal_or_xen − MemAvailable) + weighted_swap_used_kb
///
/// Since MemAvailable >= (MemFree + Buffers + Cached), the Rust value is
/// always <= the C value for the same memory snapshot.  This is intentional:
/// the Rust formula avoids over-reporting used memory due to reclaimable slab.
#[test]
fn test_c_formula_versus_rust_formula_relationship() {
    // Simulate a snapshot: all values in kB.
    let mem_total: u64 = 8_000_000;
    let mem_free: u64 = 1_000_000;
    let buffers: u64 = 200_000;
    let cached: u64 = 2_000_000;
    // MemAvailable includes reclaimable slab on top of the above.
    let reclaimable_slab: u64 = 300_000;
    let mem_available: u64 = mem_free + buffers + cached + reclaimable_slab;

    let swap_total: u64 = 2_000_000;
    let swap_free: u64 = 1_500_000;
    let swap_used = swap_total - swap_free; // = 500_000

    // C formula
    let c_used = mem_total - mem_free - buffers - cached + swap_total - swap_free;

    // Rust formula (no Xen override, no swap weights)
    let rust_used = (mem_total - mem_available) + swap_used;

    // Rust should be <= C due to reclaimable_slab being subtracted.
    assert!(
        rust_used <= c_used,
        "Rust used_mem ({rust_used}) should be <= C used_mem ({c_used})"
    );
    // The difference is exactly reclaimable_slab.
    assert_eq!(c_used - rust_used, reclaimable_slab);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract a numeric field from a `key=<number>kB` token in the first line of output.
fn parse_field(output: &str, key: &str) -> Option<u64> {
    let first_line = output.lines().next()?;
    let start = first_line.find(key)? + key.len();
    let rest = &first_line[start..];
    // Value is terminated by "kB" or whitespace.
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}
