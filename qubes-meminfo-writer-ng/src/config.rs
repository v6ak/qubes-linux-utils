// Configuration file parsing.
//
// Settings are read from a TOML file (default: /etc/qubes/meminfo-writer-ng.conf).
// The file is optional; missing or unparseable files fall back to built-in defaults.
//
// Supported keys:
//   threshold_kb  – u64 > 0  (memory change threshold in kB)
//   delay_us      – u64 > 0  (sleep interval in µs)
//
//   [[swap]]      – ordered list of swap-weight rules (first match wins):
//     glob        – shell glob pattern for the swap device/file path
//     weight      – non-negative float multiplier for used-kB

use std::collections::HashMap;
use std::fs;

use serde::Deserialize;

use crate::glob::glob_matches;

pub const DEFAULT_THRESHOLD_KB: u64 = 30_000;
pub const DEFAULT_DELAY_US: u64 = 100_000;

// ── SwapWeightRule ────────────────────────────────────────────────────────────

/// A swap-weight rule: the first matching rule for a device/file is applied.
#[derive(Debug, Clone, Deserialize)]
pub struct SwapWeightRule {
    /// Shell glob pattern matched against the swap device/file path.
    pub glob: String,
    /// Multiplier applied to the used-kB value.
    /// `0.0` = ignore this swap entirely, `1.0` = count in full.
    pub weight: f64,
}

/// Look up the weight for a swap filename using the configured rules.
/// Returns `1.0` if no rule matches (full weight by default).
pub fn swap_weight(filename: &str, rules: &[SwapWeightRule]) -> f64 {
    rules
        .iter()
        .find(|r| glob_matches(&r.glob, filename))
        .map(|r| r.weight)
        .unwrap_or(1.0)
}

// ── WeightCache ───────────────────────────────────────────────────────────────

/// Lazily-populated cache that maps swap device paths to their configured weights.
///
/// The swap rules (glob patterns) are static after startup.  Swap device paths
/// rarely change after boot.  By caching the first glob-match result for each
/// path, glob evaluation is amortised to **once per unique device path** over
/// the process lifetime rather than once per sampling tick.
pub struct WeightCache {
    rules: Vec<SwapWeightRule>,
    cache: HashMap<String, f64>,
}

impl WeightCache {
    /// Create a new cache backed by `rules`.
    pub fn new(rules: Vec<SwapWeightRule>) -> Self {
        WeightCache { rules, cache: HashMap::new() }
    }

    /// Return the weight for `filename`.
    ///
    /// On the first call for any given path the glob rules are evaluated and
    /// the result is stored; subsequent calls for the same path return the
    /// cached value without touching the rules at all.
    pub fn get(&mut self, filename: &str) -> f64 {
        if let Some(&w) = self.cache.get(filename) {
            return w;
        }
        let w = swap_weight(filename, &self.rules);
        self.cache.insert(filename.to_string(), w);
        w
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

fn default_threshold_kb() -> u64 {
    DEFAULT_THRESHOLD_KB
}
fn default_delay_us() -> u64 {
    DEFAULT_DELAY_US
}

/// Settings loaded from the TOML configuration file.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Minimum memory change (in kB) that triggers a xenstore write.
    #[serde(default = "default_threshold_kb")]
    pub threshold_kb: u64,
    /// Sleep interval between updates in microseconds.
    #[serde(default = "default_delay_us")]
    pub delay_us: u64,
    /// Ordered list of swap-weight rules (first match wins).
    #[serde(default)]
    pub swap: Vec<SwapWeightRule>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            threshold_kb: DEFAULT_THRESHOLD_KB,
            delay_us: DEFAULT_DELAY_US,
            swap: Vec::new(),
        }
    }
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse the TOML configuration file at `path`.
///
/// Returns built-in defaults when the file cannot be read or parsed, emitting
/// a warning to stderr.  Invalid field values (e.g. `threshold_kb = 0`) also
/// fall back to their defaults with a warning.
pub fn parse_config(path: &str) -> Config {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not read config file {path}: {e}; using defaults");
            return Config::default();
        }
    };

    let mut cfg: Config = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not parse config file {path}: {e}; using defaults");
            return Config::default();
        }
    };

    // Post-parse validation: reject values that are syntactically valid TOML
    // but semantically invalid for this program.
    if cfg.threshold_kb == 0 {
        eprintln!("warning: {path}: threshold_kb must be a positive integer; using default");
        cfg.threshold_kb = DEFAULT_THRESHOLD_KB;
    }
    if cfg.delay_us == 0 {
        eprintln!("warning: {path}: delay_us must be a positive integer; using default");
        cfg.delay_us = DEFAULT_DELAY_US;
    }
    for rule in &mut cfg.swap {
        if !rule.weight.is_finite() || rule.weight < 0.0 {
            eprintln!(
                "warning: {path}: weight for glob '{}' must be a non-negative finite \
                 number; using 1.0",
                rule.glob
            );
            rule.weight = 1.0;
        }
    }

    cfg
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal RAII temp-file helper (no external crate needed).
    struct TempFile {
        path: std::path::PathBuf,
    }
    impl TempFile {
        fn new(content: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "qubes-meminfo-cfg-test-{}-{}.toml",
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

    #[test]
    fn test_swap_weight_zero_ignores_device() {
        let rules = vec![SwapWeightRule { glob: "*".into(), weight: 0.0 }];
        assert_eq!(swap_weight("/dev/sda2", &rules), 0.0);
    }

    #[test]
    fn test_swap_weight_no_match_uses_default() {
        let rules = vec![SwapWeightRule { glob: "/dev/zram*".into(), weight: 0.5 }];
        assert_eq!(swap_weight("/dev/sda2", &rules), 1.0); // no rule matched
    }

    // ── parse_config ──────────────────────────────────────────────────────────

    #[test]
    fn test_parse_config_defaults_on_missing_file() {
        let cfg = parse_config("/nonexistent/path/config.toml");
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
        assert!(cfg.swap.is_empty());
    }

    #[test]
    fn test_parse_config_empty_file_uses_defaults() {
        let tmp = TempFile::new("");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
    }

    #[test]
    fn test_parse_config_scalar_values() {
        let tmp = TempFile::new("# comment\nthreshold_kb = 50000\ndelay_us = 200000\n");
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
        assert_eq!(cfg.swap.len(), 2);
        assert_eq!(cfg.swap[0].glob, "/dev/zram*");
        assert_eq!(cfg.swap[0].weight, 0.5);
        assert_eq!(cfg.swap[1].glob, "*");
        assert_eq!(cfg.swap[1].weight, 1.0);
    }

    #[test]
    fn test_parse_config_threshold_zero_uses_default() {
        let tmp = TempFile::new("threshold_kb = 0\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
    }

    #[test]
    fn test_parse_config_delay_zero_uses_default() {
        let tmp = TempFile::new("delay_us = 0\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
    }

    #[test]
    fn test_parse_config_bad_syntax_uses_defaults() {
        // toml::from_str will reject this as a parse error
        let tmp = TempFile::new("threshold_kb = notanumber\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.threshold_kb, DEFAULT_THRESHOLD_KB);
        assert_eq!(cfg.delay_us, DEFAULT_DELAY_US);
    }

    #[test]
    fn test_parse_config_negative_weight_reset_to_one() {
        let tmp = TempFile::new("[[swap]]\nglob = \"*\"\nweight = -1.0\n");
        let cfg = parse_config(tmp.path_str());
        assert_eq!(cfg.swap.len(), 1);
        assert_eq!(cfg.swap[0].weight, 1.0); // fixed by post-parse validation
    }

    #[test]
    fn test_parse_config_comments_and_whitespace() {
        let tmp = TempFile::new(
            "# leading comment\n\
             threshold_kb = 10000  # inline comment not supported in TOML values\n",
        );
        // TOML does not support inline comments after values on the same line for
        // string values, but numeric values followed by # are a parse error.
        // A valid TOML file with just the number should work.
        let tmp2 = TempFile::new(
            "# just a threshold\n\
             threshold_kb = 10000\n",
        );
        let cfg = parse_config(tmp2.path_str());
        assert_eq!(cfg.threshold_kb, 10_000);
        // tmp is dropped (may have parse errors – that's fine, just a sanity check)
        let _ = tmp;
    }

    // ── WeightCache ───────────────────────────────────────────────────────────

    #[test]
    fn test_weight_cache_returns_correct_weights() {
        let rules = vec![
            SwapWeightRule { glob: "/dev/zram*".into(), weight: 0.5 },
            SwapWeightRule { glob: "*".into(), weight: 1.0 },
        ];
        let mut cache = WeightCache::new(rules);
        assert_eq!(cache.get("/dev/zram0"), 0.5);
        assert_eq!(cache.get("/dev/sda2"), 1.0);
    }

    #[test]
    fn test_weight_cache_no_rules_defaults_to_one() {
        let mut cache = WeightCache::new(vec![]);
        assert_eq!(cache.get("/dev/sda2"), 1.0);
    }

    #[test]
    fn test_weight_cache_repeated_lookup_is_consistent() {
        // Second lookup for the same path must return the same value (cached).
        let rules = vec![SwapWeightRule { glob: "/dev/zram*".into(), weight: 0.25 }];
        let mut cache = WeightCache::new(rules);
        let first = cache.get("/dev/zram0");
        let second = cache.get("/dev/zram0"); // cached path
        assert_eq!(first, second);
        assert_eq!(first, 0.25);
    }

    #[test]
    fn test_weight_cache_different_paths_independent() {
        let rules = vec![
            SwapWeightRule { glob: "/dev/zram*".into(), weight: 0.5 },
            SwapWeightRule { glob: "*".into(), weight: 1.0 },
        ];
        let mut cache = WeightCache::new(rules);
        // Interleave lookups to ensure the cache doesn't confuse paths.
        assert_eq!(cache.get("/dev/zram0"), 0.5);
        assert_eq!(cache.get("/dev/sda2"), 1.0);
        assert_eq!(cache.get("/dev/zram1"), 0.5); // different zram device, same rule
        assert_eq!(cache.get("/dev/sda2"), 1.0); // already cached
    }
}
