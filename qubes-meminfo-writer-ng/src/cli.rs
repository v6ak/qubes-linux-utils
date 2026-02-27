// Command-line argument parsing.

use std::process;

use crate::config::{DEFAULT_DELAY_US, DEFAULT_THRESHOLD_KB};

/// Default path to the configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/qubes/meminfo-writer-ng.conf";

// ── OutputMode ────────────────────────────────────────────────────────────────

/// Output mode: controls where computed values and xenstore writes go.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputMode {
    /// Print computed values to stdout only; do not write to xenstore.
    Print,
    /// Write to xenstore only; no stdout output (default).
    Xenstore,
    /// Write to xenstore AND print computed values to stdout.
    Both,
}

// ── CliArgs ───────────────────────────────────────────────────────────────────

/// Parsed command-line arguments.
pub struct CliArgs {
    /// Path to the configuration file.
    pub config_path: String,
    /// Selected output mode.
    pub output_mode: OutputMode,
    /// When `true`, run exactly one sampling iteration then exit.
    /// Useful for scripting, debugging, and integration testing.
    pub once: bool,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn print_usage(prog: &str) {
    eprintln!("Usage: {prog} [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --config <path>        Configuration file [default: {DEFAULT_CONFIG_PATH}]");
    eprintln!("  --output <mode>        Output mode (default: xenstore):");
    eprintln!("    print                Print computed values to stdout; no xenstore writes");
    eprintln!("    xenstore             Write to xenstore only (silent)");
    eprintln!("    both                 Write to xenstore and print to stdout");
    eprintln!("  --once                 Run one iteration and exit (useful for debugging)");
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

pub fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("qubes-meminfo-writer-ng");

    let mut config_path = DEFAULT_CONFIG_PATH.to_string();
    let mut output_mode = OutputMode::Xenstore;
    let mut once = false;

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
                        eprintln!(
                            "error: unknown --output mode '{other}'; \
                             expected print|xenstore|both"
                        );
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
            "--once" => once = true,
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

    CliArgs { config_path, output_mode, once }
}
