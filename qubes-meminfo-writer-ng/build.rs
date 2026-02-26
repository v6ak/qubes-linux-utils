// build.rs – link libxenstore when the "xenstore" Cargo feature is enabled.
//
// The "xenstore" feature is on by default (see Cargo.toml).  To build without
// xenstore support (e.g. for testing on a non-Xen machine) use:
//
//   cargo build --no-default-features
//
// When xenstore is enabled this script:
//   1. emits `cargo:rustc-link-lib=xenstore` so the linker links it, and
//   2. sets the `has_xenstore` cfg flag so xenstore.rs is compiled in.

fn main() {
    // Declare `has_xenstore` as a valid cfg so Rust's check-cfg lint does not
    // warn about it in source files.
    println!("cargo::rustc-check-cfg=cfg(has_xenstore)");

    if std::env::var("CARGO_FEATURE_XENSTORE").is_ok() {
        println!("cargo:rustc-link-lib=xenstore");
        println!("cargo:rustc-cfg=has_xenstore");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
