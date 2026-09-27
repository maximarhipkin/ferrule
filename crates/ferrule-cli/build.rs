//! M36: the release build the updater fetches for this binary. The Linux
//! releases are static musl builds, which run on any Linux, so a glibc
//! build (from source) updates to them too.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!(
        "cargo:rustc-env=FERRULE_TARGET={}",
        target.replace("-linux-gnu", "-linux-musl")
    );
    println!("cargo:rerun-if-changed=build.rs");
}
