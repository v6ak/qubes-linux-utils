// Memory usage computation and xenstore update heuristic.

use std::fs;

/// Path to the Xen balloon driver's view of current memory in kB.
const XEN_CURRENT_KB_PATH: &str =
    "/sys/devices/system/xen_memory/xen_memory0/info/current_kb";

// ── Xen helpers ───────────────────────────────────────────────────────────────

/// Read the Xen balloon driver's current memory allocation in kB.
///
/// Returns `None` when the path does not exist (non-Xen machine), is
/// unreadable, or contains zero (meaning "not set").
pub fn read_xen_current_kb() -> Option<u64> {
    fs::read_to_string(XEN_CURRENT_KB_PATH)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v != 0)
}

// ── Memory calculation ────────────────────────────────────────────────────────

/// Return the total "used" memory in kB: used RAM + weighted used swap.
///
/// When `xen_current_kb` is `Some`, it replaces the kernel-reported total
/// memory (the Xen balloon driver may have shrunk the VM's allocation below
/// what the OS thinks is installed).
///
/// `weighted_swap_used_kb` is the caller-supplied sum of each swap device's
/// used-kB multiplied by its configured weight.  Use
/// [`swap::weighted_swap_used_kb`](crate::swap::weighted_swap_used_kb) to
/// compute it.  Passing the pre-computed sum avoids calling `swap_weight`
/// twice when the print mode also needs per-entry weights for display.
pub fn compute_used_memory(
    total_memory_kb: u64,
    available_memory_kb: u64,
    xen_current_kb: Option<u64>,
    weighted_swap_used_kb: u64,
) -> u64 {
    let effective_total_kb = xen_current_kb.unwrap_or(total_memory_kb);
    // "used RAM" = effective total − MemAvailable (accounts for free pages,
    // buffers, and reclaimable cache).
    let used_ram_kb = effective_total_kb.saturating_sub(available_memory_kb);
    used_ram_kb + weighted_swap_used_kb
}

// ── Update heuristic ──────────────────────────────────────────────────────────

/// Decide whether the new value is worth sending to xenstore.
///
/// Mirrors the heuristic of the original C implementation:
/// * Always send on the first update (`prev == 0`).
/// * Send when the absolute change exceeds `threshold_kb`.
/// * Send when memory is under pressure (> ~77 % of total) and usage is
///   rising and the change exceeds half the threshold.
pub fn should_update(
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
    // send an early update when memory usage is rising and already above
    // ~77 % of total (10/13 ≈ 0.769).  The +12 in the divisor biases the
    // integer division toward the same boundary as the floating-point
    // equivalent.
    if used_mem_kb > prev_used_mem_kb
        && used_mem_kb * 13 > (total_mem_kb + 12) * 10
        && diff > threshold_kb / 2
    {
        return true;
    }
    false
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_used_memory ───────────────────────────────────────────────────

    #[test]
    fn test_no_swap_no_xen() {
        assert_eq!(compute_used_memory(8_000_000, 6_000_000, None, 0), 2_000_000);
    }

    #[test]
    fn test_with_weighted_swap() {
        // used_ram = 8M - 6M = 2M, swap = 1024+512 = 1536
        assert_eq!(compute_used_memory(8_000_000, 6_000_000, None, 1_536), 2_001_536);
    }

    #[test]
    fn test_xen_override_reduces_effective_total() {
        // Xen says 4 GB; available is 6 GB → saturates to 0 used RAM.
        assert_eq!(compute_used_memory(8_000_000, 6_000_000, Some(4_000_000), 0), 0);
    }

    #[test]
    fn test_xen_override_within_available() {
        // Xen says 7 GB; available is 6 GB → 1 GB used RAM.
        assert_eq!(compute_used_memory(8_000_000, 6_000_000, Some(7_000_000), 0), 1_000_000);
    }

    #[test]
    fn test_zero_available_gives_all_used() {
        assert_eq!(compute_used_memory(4_000_000, 0, None, 0), 4_000_000);
    }

    // ── should_update ─────────────────────────────────────────────────────────

    #[test]
    fn test_first_call_always_updates() {
        assert!(should_update(1_000_000, 0, 30_000, 8_000_000));
    }

    #[test]
    fn test_large_change_triggers() {
        assert!(should_update(1_100_000, 1_000_000, 30_000, 8_000_000));
    }

    #[test]
    fn test_exactly_at_threshold_does_not_trigger() {
        // diff == threshold_kb: condition is `diff > threshold_kb` (strict), so no update.
        assert!(!should_update(1_030_000, 1_000_000, 30_000, 8_000_000));
    }

    #[test]
    fn test_small_change_no_pressure_no_update() {
        assert!(!should_update(1_010_000, 1_000_000, 30_000, 8_000_000));
    }

    #[test]
    fn test_pressure_heuristic_rising_usage() {
        let total = 8_000_000u64;
        let used = total * 80 / 100; // 80 % → above the ~77 % threshold
        let prev = used - 20_000; // rising, diff=20_000 > threshold/2=15_000
        assert!(should_update(used, prev, 30_000, total));
    }

    #[test]
    fn test_pressure_heuristic_decreasing_no_update() {
        // Pressure heuristic only fires when usage is INCREASING.
        let total = 8_000_000u64;
        let used = total * 80 / 100;
        let prev = used + 20_000; // usage fell → decreasing
        assert!(!should_update(used, prev, 30_000, total));
    }

    #[test]
    fn test_pressure_heuristic_below_77pct_no_update() {
        // Usage below the ~77 % pressure threshold → normal threshold applies.
        let total = 8_000_000u64;
        let used = total * 50 / 100; // 50 % → below threshold
        let prev = used - 20_000;
        assert!(!should_update(used, prev, 30_000, total));
    }
}
