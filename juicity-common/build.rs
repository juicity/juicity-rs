use std::process::Command;

fn main() {
    // ---- Build timestamp ----
    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    println!("cargo:rustc-env=JUICITY_BUILD_TIMESTAMP={}", timestamp);

    // ---- Git commit hash ----
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=JUICITY_GIT_HASH={}", git_hash);

    // Rerun build script only when Git HEAD changes (or if build.rs itself changes)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
