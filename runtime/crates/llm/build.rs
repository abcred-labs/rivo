#[path = "src/release_version.rs"]
mod release_version;

fn main() {
    let explicit_version = std::env::var("RIVO_RELEASE_VERSION").ok();
    let release_version = release_version::resolve_release_version(
        env!("CARGO_PKG_VERSION"),
        explicit_version.as_deref(),
    )
    .unwrap_or_else(|error| panic!("invalid Rivo release version configuration: {error}"));

    println!("cargo:rustc-env=RIVO_LLM_RELEASE_VERSION={release_version}");
    println!("cargo:rerun-if-env-changed=RIVO_RELEASE_VERSION");
    println!("cargo:rerun-if-changed=src/release_version.rs");
}
