//! Build-time capture for the About page: the short git hash of
//! the checkout being built and the target triple. Hand-rolled
//! (no vergen dependency); degrades gracefully to "unknown" when
//! git or a checkout is unavailable, a source tarball build must
//! never fail because of this.

use std::process::Command;

fn main() {
    // CI may pin the build code explicitly (e.g. to the tagged commit);
    // it wins over whatever checkout git sees.
    println!("cargo:rerun-if-env-changed=ZAPRUST_BUILD_SHA");
    let hash = std::env::var("ZAPRUST_BUILD_SHA")
        .ok()
        .map(|s| s.trim().chars().take(7).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(git_short_hash);
    println!("cargo:rustc-env=ZAPRUST_GIT_HASH={hash}");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=ZAPRUST_TARGET={target}");
    // Re-run when HEAD moves.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn git_short_hash() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}
