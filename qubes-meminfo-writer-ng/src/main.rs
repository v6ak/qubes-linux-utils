// qubes-meminfo-writer-ng – Qubes memory information reporter (Rust rewrite).
//
// This program periodically reads system memory information and reports the
// amount of "used" memory (RAM + swap) to xenstore at "memory/meminfo", so
// that the qmemman daemon in dom0 can perform memory ballooning.
//
// CLI flags:
//   --output <mode>    print | xenstore | both  (default: xenstore)
//   --config <path>    use an alternative configuration file

mod cli;
mod config;
mod glob;
mod mem;
mod swap;
mod xenstore;

use std::process;
use std::thread;
use std::time::Duration;

use sysinfo::System;

use cli::OutputMode;
use config::swap_weight;
use mem::{compute_used_memory, read_xen_current_kb, should_update};
use swap::read_swap_entries;

// Xenstore key written by this program (same as the original C implementation).
const XENSTORE_MEMINFO_PATH: &str = "memory/meminfo";

fn main() {
    let cli = cli::parse_args();
    let cfg = config::parse_config(&cli.config_path);

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

        // Pre-compute per-entry weights once so they can be used for both the
        // weighted total and the per-entry debug output without a second lookup.
        let entry_weights: Vec<f64> = swap_entries
            .iter()
            .map(|e| swap_weight(&e.filename, &cfg.swap))
            .collect();
        let swap_used_kb_weighted: u64 = swap_entries
            .iter()
            .zip(&entry_weights)
            .map(|(e, &w)| (e.used_kb as f64 * w) as u64)
            .sum();

        let used_mem_kb =
            compute_used_memory(total_memory_kb, available_memory_kb, xen_current_kb, swap_used_kb_weighted);

        if use_print {
            let swap_total_kb: u64 = swap_entries.iter().map(|e| e.total_kb).sum();
            let swap_used_kb_raw: u64 = swap_entries.iter().map(|e| e.used_kb).sum();
            println!(
                "total_mem={total_memory_kb}kB \
                 available_mem={available_memory_kb}kB \
                 swap_total={swap_total_kb}kB \
                 swap_used_raw={swap_used_kb_raw}kB \
                 xen_current={}kB \
                 used_mem={used_mem_kb}kB",
                xen_current_kb.unwrap_or(0)
            );
            for (entry, &w) in swap_entries.iter().zip(&entry_weights) {
                let used_weighted = (entry.used_kb as f64 * w) as u64;
                println!(
                    "  swap: {} total={}kB used_raw={}kB weight={w:.3} used_weighted={used_weighted}kB",
                    entry.filename, entry.total_kb, entry.used_kb,
                );
            }
        }

        let data = used_mem_kb.to_string();
        let will_write =
            should_update(used_mem_kb, prev_used_mem_kb, cfg.threshold_kb, total_memory_kb);
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

