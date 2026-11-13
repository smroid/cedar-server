// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// Opt-in build info: with CEDAR_BUILD_INFO=1 the version gets a
// " (<git describe> <user>)" suffix, e.g. "1.7.5 (77349c2e-dirty alex)".
// Without it the version is just the Cargo.toml version.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=CEDAR_BUILD_INFO");
    let mut suffix = String::new();
    if std::env::var("CEDAR_BUILD_INFO").is_ok_and(|v| v == "1") {
        let sha = git(&["describe", "--always", "--dirty", "--abbrev=8"]);
        let user = std::env::var("USER").unwrap_or_default();
        suffix = format!(" ({} {})", sha.as_deref().unwrap_or("unknown"), user);
        // Rebuild on commits and edits so the sha and -dirty stay current.
        // --git-path resolves correctly inside git worktrees too.
        for path in
            git(&["rev-parse", "--git-path", "HEAD", "--git-path", "index"])
                .unwrap_or_default()
                .lines()
        {
            println!("cargo:rerun-if-changed={path}");
        }
        println!("cargo:rerun-if-changed=src");
    }
    println!("cargo:rustc-env=CEDAR_VERSION_SUFFIX={suffix}");
}
