fn main() {
    // Expose target triple as a compile-time env var for the version string.
    // CARGO_CFG_* vars are only available in build.rs, not in main code.
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
