// SPDX-License-Identifier: MPL-2.0

//! Embed the git commit the server is built from, logged at startup: `QDW_COMMIT` when set
//! (package builds from a source tarball, which has no git metadata), otherwise from git.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=QDW_COMMIT");
    let commit = std::env::var("QDW_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            // Rebuild when HEAD moves.
            println!("cargo:rerun-if-changed=../.git/HEAD");
            println!("cargo:rerun-if-changed=../.git/index");
            let head = Command::new("git")
                .args(["rev-parse", "--short=10", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())?;
            let mut commit = String::from_utf8(head.stdout).ok()?.trim().to_owned();
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .ok()
                .is_some_and(|o| !o.stdout.is_empty());
            if dirty {
                commit.push_str("-dirty");
            }
            Some(commit)
        });
    println!(
        "cargo:rustc-env=QDW_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
}
