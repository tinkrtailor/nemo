use std::process::Command;

fn main() {
    // Attempt to get the short SHA of the current git commit.
    // If git is unavailable or we're not in a repo, silently skip
    // so that `option_env!("BUILD_SHA")` returns `None` at compile time.
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();

    if !sha.is_empty() {
        println!("cargo:rustc-env=BUILD_SHA={sha}");
    }

    // Re-run this build script when the checked-out commit changes,
    // so iterative local builds pick up the new SHA.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/");
}
