use std::env;

#[path = "src/release_version.rs"]
mod release_version;

fn main() {
    let release_version = release_version::resolve_release_version(
        env!("CARGO_PKG_VERSION"),
        env::var("RIVO_RELEASE_VERSION").ok(),
        env::var("RIVO_BUILD_NUMBER").ok(),
    )
    .unwrap_or_else(|error| panic!("invalid Rivo release version configuration: {error}"));
    println!("cargo:rustc-env=RIVO_RELEASE_VERSION={release_version}");
    println!("cargo:rerun-if-env-changed=RIVO_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=RIVO_BUILD_NUMBER");
    println!("cargo:rerun-if-changed=src/release_version.rs");
    // Build provenance lets hosts tell two binaries with the same release version apart.
    let commit = build_commit();
    println!(
        "cargo:rustc-env=RIVO_BUILD_COMMIT={}",
        commit.as_deref().unwrap_or("")
    );
    let display = match &commit {
        Some(commit) => format!("{release_version} ({commit})"),
        None => release_version.clone(),
    };
    println!("cargo:rustc-env=RIVO_VERSION_DISPLAY={display}");
    println!("cargo:rerun-if-changed=../../../assets/brand/rivo.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon_path = std::fs::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/brand/rivo.ico"),
    )
    .expect("Rivo icon asset must exist");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon_path.to_str().expect("Rivo icon path must be UTF-8"))
        .set("CompanyName", "Rivo")
        .set("FileDescription", "Rivo Agent CLI")
        .set("FileVersion", &release_version)
        .set("InternalName", "rivo")
        .set("LegalCopyright", "Copyright (c) 2026 Rivo contributors")
        .set("OriginalFilename", "rivo.exe")
        .set("ProductName", "Rivo")
        .set("ProductVersion", &release_version)
        .set_language(0x0409);

    resource
        .compile()
        .expect("failed to compile Windows version resource");
}

/// Short HEAD commit, suffixed with `-dirty` when tracked files differ. `None` outside a git checkout.
fn build_commit() -> Option<String> {
    println!("cargo:rerun-if-env-changed=RIVO_BUILD_COMMIT");
    if let Ok(commit) = env::var("RIVO_BUILD_COMMIT") {
        return (!commit.trim().is_empty()).then(|| commit.trim().to_owned());
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "--short=7", "HEAD"])?;
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty());
    Some(if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    })
}
