//! Stamps the binary with its commit (`COLLIE_COMMIT`), which two daemons compare before one runs work for the other.
//! `unknown` outside git (a source tarball, a copy synced without `.git`).

use std::path::Path;
use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        let o = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
    };
    println!("cargo:rustc-env=COLLIE_COMMIT={}", git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into()));
    println!("cargo:rerun-if-changed=build.rs");
    // `--git-path`, not `.git/…`: in a worktree `.git` is a file, HEAD is per worktree and the refs are shared.
    let refs = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r]));
    // Only existing files: cargo reruns a script on every build while a watched path is missing.
    // The reflog too: every commit and checkout touches it, even once `git pack-refs` moved the branch's ref away.
    let watched = [git(&["rev-parse", "--git-path", "HEAD"]), refs, git(&["rev-parse", "--git-path", "packed-refs"]), git(&["rev-parse", "--git-path", "logs/HEAD"])];
    for p in watched.into_iter().flatten().filter(|p| Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={p}");
    }
}
