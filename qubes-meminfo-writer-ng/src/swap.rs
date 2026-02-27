// /proc/swaps parsing.

use std::{fs, io};

// ── SwapEntry ─────────────────────────────────────────────────────────────────

/// Swap usage for a single device or file as reported by `/proc/swaps`.
#[derive(Debug, PartialEq)]
pub struct SwapEntry {
    /// Device path or file name (first column of `/proc/swaps`).
    pub filename: String,
    /// Total size in kB.
    pub total_kb: u64,
    /// Amount in use in kB.
    pub used_kb: u64,
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse the text content of `/proc/swaps` and return one [`SwapEntry`] per
/// active swap area.
///
/// The kernel file format is:
/// ```text
/// Filename                        Type    Size    Used    Priority
/// /dev/sda2                       partition 8388604 0 -2
/// /swapfile                       file    2097148 1024 -3
/// ```
///
/// The first (header) line is skipped.  Blank or otherwise unparseable lines
/// are silently ignored.
pub fn parse_swap_content(content: &str) -> Vec<SwapEntry> {
    let mut entries = Vec::new();
    for line in content.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let filename = match fields.next() {
            Some(f) => f.to_string(),
            None => continue, // blank line
        };
        let _swap_type = fields.next(); // "partition" or "file" – ignored
        let total_kb: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let used_kb: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        entries.push(SwapEntry { filename, total_kb, used_kb });
    }
    entries
}

/// Read `/proc/swaps` and return one [`SwapEntry`] per active swap area.
pub fn read_swap_entries() -> io::Result<Vec<SwapEntry>> {
    fs::read_to_string("/proc/swaps").map(|c| parse_swap_content(&c))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SwapWeightRule;

    fn make_rule(glob: &str, weight: f64) -> SwapWeightRule {
        SwapWeightRule { glob: glob.into(), weight }
    }

    /// Compute the total weighted used-swap in kB.  Used only in tests.
    fn weighted_swap_used_kb(entries: &[SwapEntry], rules: &[SwapWeightRule]) -> u64 {
        use crate::config::swap_weight;
        entries
            .iter()
            .map(|e| (e.used_kb as f64 * swap_weight(&e.filename, rules)) as u64)
            .sum()
    }

    // ── parse_swap_content ────────────────────────────────────────────────────

    #[test]
    fn test_parse_basic() {
        let content = "Filename\t\t\tType\t\tSize\tUsed\tPriority\n\
                       /dev/sda2               partition 8388604 0 -2\n\
                       /swapfile               file 2097148 1024 -3\n";
        let entries = parse_swap_content(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            SwapEntry { filename: "/dev/sda2".into(), total_kb: 8_388_604, used_kb: 0 }
        );
        assert_eq!(
            entries[1],
            SwapEntry { filename: "/swapfile".into(), total_kb: 2_097_148, used_kb: 1_024 }
        );
    }

    #[test]
    fn test_parse_only_header_is_empty() {
        let content = "Filename\t\t\tType\t\tSize\tUsed\tPriority\n";
        assert!(parse_swap_content(content).is_empty());
    }

    #[test]
    fn test_parse_blank_lines_skipped() {
        let content = "Filename\t\t\tType\t\tSize\tUsed\tPriority\n\
                       \n\
                       /dev/sda2 partition 1000 0 -2\n";
        let entries = parse_swap_content(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "/dev/sda2");
    }

    #[test]
    fn test_parse_missing_numeric_fields_default_to_zero() {
        // A line with only a filename (all numeric fields absent).
        let content = "Filename\t\t\tType\t\tSize\tUsed\tPriority\n\
                       /dev/sda2\n";
        let entries = parse_swap_content(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].total_kb, 0);
        assert_eq!(entries[0].used_kb, 0);
    }

    // ── weighted_swap_used_kb ─────────────────────────────────────────────────

    #[test]
    fn test_weighted_no_entries() {
        assert_eq!(weighted_swap_used_kb(&[], &[]), 0);
    }

    #[test]
    fn test_weighted_full_weight_default() {
        let entries = vec![
            SwapEntry { filename: "/dev/sda2".into(), total_kb: 1000, used_kb: 500 },
        ];
        assert_eq!(weighted_swap_used_kb(&entries, &[]), 500);
    }

    #[test]
    fn test_weighted_half_weight() {
        let entries =
            vec![SwapEntry { filename: "/dev/zram0".into(), total_kb: 1000, used_kb: 1000 }];
        let rules = vec![make_rule("/dev/zram*", 0.5)];
        assert_eq!(weighted_swap_used_kb(&entries, &rules), 500);
    }

    #[test]
    fn test_weighted_zero_weight_excludes_device() {
        let entries =
            vec![SwapEntry { filename: "/dev/sda2".into(), total_kb: 1000, used_kb: 1000 }];
        let rules = vec![make_rule("*", 0.0)];
        assert_eq!(weighted_swap_used_kb(&entries, &rules), 0);
    }

    #[test]
    fn test_weighted_multiple_entries_mixed_weights() {
        let entries = vec![
            SwapEntry { filename: "/dev/zram0".into(), total_kb: 1000, used_kb: 1000 },
            SwapEntry { filename: "/dev/sda2".into(), total_kb: 2000, used_kb: 200 },
        ];
        let rules = vec![make_rule("/dev/zram*", 0.5), make_rule("*", 1.0)];
        // zram0: 1000 * 0.5 = 500, sda2: 200 * 1.0 = 200
        assert_eq!(weighted_swap_used_kb(&entries, &rules), 700);
    }

    // ── read_swap_entries (live /proc/swaps) ──────────────────────────────────

    #[test]
    fn test_live_proc_swaps_invariants() {
        if let Ok(entries) = read_swap_entries() {
            for e in &entries {
                assert!(!e.filename.is_empty(), "filename must not be empty");
                assert!(e.used_kb <= e.total_kb, "used ≤ total for {}", e.filename);
            }
        }
        // Silently skip when /proc/swaps is unavailable (non-Linux CI runners).
    }
}
