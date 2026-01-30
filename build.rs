use std::process::Command;

fn main() {
    // Check if version is provided via environment variable (for Docker builds)
    let version = std::env::var("GIT_VERSION").unwrap_or_else(|_| {
        // Get version from git tag, fallback to "dev" if not in a git repo or no tags
        Command::new("git")
            .args(["describe", "--tags", "--always", "--dirty"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    String::from_utf8(output.stdout).ok()
                } else {
                    None
                }
            })
            .map_or_else(|| "dev".to_string(), |s| s.trim().to_string())
    });

    println!("cargo:rustc-env=GIT_VERSION={version}");

    // Rerun if git state changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-env-changed=GIT_VERSION");
}
