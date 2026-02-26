// build.rs – detect libxenstore at build time.
//
// When libxenstore is found (via pkg-config or by looking for the shared
// library), this script:
//   1. emits `cargo:rustc-link-lib=xenstore` so the linker links it, and
//   2. sets the `has_xenstore` cfg flag so the main source can conditionally
//      compile xenstore support.
//
// When xenstore is absent the binary still compiles but only operates in
// --debug mode (it will print an error and exit if invoked without --debug).

fn check_pkg_config() -> bool {
    // Try `pkg-config --exists xenstore` first.
    std::process::Command::new("pkg-config")
        .args(["--exists", "xenstore"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_library_file() -> bool {
    // Common installation paths for libxenstore.so on Fedora / Debian / Arch.
    let candidates = [
        "/usr/lib64/libxenstore.so",
        "/usr/lib/x86_64-linux-gnu/libxenstore.so",
        "/usr/lib/aarch64-linux-gnu/libxenstore.so",
        "/usr/lib/arm-linux-gnueabihf/libxenstore.so",
        "/usr/lib/i386-linux-gnu/libxenstore.so",
        "/usr/lib/libxenstore.so",
    ];
    candidates
        .iter()
        .any(|p| std::path::Path::new(p).exists())
}

fn main() {
    // Declare `has_xenstore` as a valid cfg so Rust's check-cfg lint does not
    // warn about it in source files.
    println!("cargo::rustc-check-cfg=cfg(has_xenstore)");

    if check_pkg_config() || check_library_file() {
        // Tell Cargo to link libxenstore and expose `has_xenstore` to the
        // Rust code.
        println!("cargo:rustc-link-lib=xenstore");
        println!("cargo:rustc-cfg=has_xenstore");
    }

    // Re-run this script if the pkg-config database changes (e.g. after
    // installing xen-devel / libxen-dev).
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
}
