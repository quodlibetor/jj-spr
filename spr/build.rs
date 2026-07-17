/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bakes the commit jj-spr was built from into the binary, for `--version`.

use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=JJ_SPR_BUILD_REV");

    let version = match build_rev() {
        Some(rev) => format!("{} ({})", env!("CARGO_PKG_VERSION"), rev),
        None => env!("CARGO_PKG_VERSION").to_string(),
    };
    println!("cargo:rustc-env=JJ_SPR_VERSION={version}");
}

/// The revision to report, either handed to us by the build system or asked of
/// git. `None` when there is nothing to ask: a build from a published tarball,
/// or a source tree copied out of its repository.
fn build_rev() -> Option<String> {
    if let Ok(rev) = std::env::var("JJ_SPR_BUILD_REV") {
        let rev = rev.trim().to_string();
        if !rev.is_empty() {
            return Some(rev);
        }
    }

    // git's HEAD is the parent of the working-copy commit in a colocated jj
    // repository, so in one this names `@-` and the dirty marker covers the
    // working-copy commit's own changes.
    let mut rev = git(&["rev-parse", "--short", "HEAD"])?;
    if !git(&["status", "--porcelain"])?.is_empty() {
        rev.push_str(" dirty");
    }

    watch_head();

    Some(rev)
}

/// Rebuild when the commit HEAD names changes. Uncommitted changes are not
/// watched: cargo already rebuilds when a source file changes, and the marker
/// is a description of that rebuild rather than a reason for one.
fn watch_head() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = Path::new(&git_dir);

    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    // HEAD alone is not enough: committing on a branch leaves it pointing at
    // the same ref and only moves the ref.
    let head_log = git_dir.join("logs").join("HEAD");
    if head_log.exists() {
        println!("cargo:rerun-if-changed={}", head_log.display());
    }
}

/// The trimmed stdout of a successful git invocation, or `None` if git is
/// missing or unhappy — no build should fail over a version string.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}
