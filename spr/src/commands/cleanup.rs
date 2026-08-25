/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashSet;
use std::process::Stdio;

use crate::{error::Result, github::ClosedPullRequest, output::output, reopen};

#[derive(Debug, clap::Parser)]
pub struct CleanupOptions {
    /// Actually delete the orphan branches (default is list-only)
    #[clap(long)]
    confirm: bool,
}

/// Extract branch names from refs that match the SPR remote prefix.
///
/// Given refs like `refs/remotes/origin/spr/user/my-feature`, extracts
/// `spr/user/my-feature`.
fn extract_spr_branch_names(
    all_refs: &HashSet<String>,
    remote_name: &str,
    branch_prefix: &str,
) -> Vec<String> {
    let remote_prefix = format!("refs/remotes/{}/{}", remote_name, branch_prefix);
    let strip_len = "refs/remotes/".len() + remote_name.len() + 1;

    all_refs
        .iter()
        .filter(|r| r.starts_with(&remote_prefix))
        .map(|r| r[strip_len..].to_string())
        .collect()
}

/// Find SPR branches that are not referenced by any open PR.
fn find_orphan_branches<'a>(
    spr_branches: &'a [String],
    open_pr_branches: &HashSet<String>,
) -> Vec<&'a String> {
    spr_branches
        .iter()
        .filter(|b| !open_pr_branches.contains(*b))
        .collect()
}

pub async fn cleanup(
    opts: CleanupOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    // Before anything is deleted, and deliberately so. A pull request GitHub
    // closed when its base branch went missing still has a head branch, and that
    // branch belongs to no *open* pull request — so the sweep below would take it
    // for an orphan and delete it, which is the one thing that would make the
    // pull request unrecoverable: a head branch has to come back at the exact
    // commit the pull request records to reopen it
    // (`GitHubRule::AResurrectedBaseRefNeedsNoParticularCommit`), and once the
    // sweep has deleted it, nothing here knows that commit any more.
    put_back_closed_pull_requests(&opts, jj, gh, config).await?;

    output("🔍", "Finding orphan SPR branches...")?;

    let all_refs = jj.get_all_ref_names()?;
    let spr_branches =
        extract_spr_branch_names(&all_refs, &config.remote_name, &config.branch_prefix);

    if spr_branches.is_empty() {
        output("✨", "No SPR branches found. Nothing to clean up.")?;
        return Ok(());
    }

    let open_pr_branches = gh.get_open_pr_branch_names().await?;
    let orphan_branches = find_orphan_branches(&spr_branches, &open_pr_branches);

    if orphan_branches.is_empty() {
        output(
            "✨",
            &format!(
                "All {} SPR branch(es) belong to open PRs. Nothing to clean up.",
                spr_branches.len()
            ),
        )?;
        return Ok(());
    }

    output(
        "🗑️",
        &format!(
            "Found {} orphan SPR branch(es) (out of {} total):",
            orphan_branches.len(),
            spr_branches.len()
        ),
    )?;

    let term = console::Term::stdout();
    for branch in &orphan_branches {
        term.write_line(&format!("     {}", console::style(*branch).dim()))?;
    }

    if !opts.confirm {
        output("💡", "Run with --confirm to delete these branches.")?;
        return Ok(());
    }

    output("🧹", "Deleting orphan branches...")?;

    for branch in &orphan_branches {
        let result = tokio::process::Command::new("git")
            .arg("push")
            .arg("--no-verify")
            .arg("--delete")
            .arg("--")
            .arg(&config.remote_name)
            .arg(format!("refs/heads/{}", branch))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await;

        match result {
            Ok(status) if status.status.success() => {
                output("✅", &format!("Deleted {}", branch))?;
            }
            _ => {
                output(
                    "⚠️",
                    &format!("Failed to delete {} (may already be gone)", branch),
                )?;
            }
        }
    }

    output("✨", "Cleanup complete.")?;
    Ok(())
}

/// Reopen the pull requests GitHub closed when a branch went missing.
///
/// `cleanup` is where somebody looks when GitHub's merge queue has left a stack
/// in a state nobody asked for, so this is the command that has to notice. What
/// it does is the first half of the repair — the branch back, the pull request
/// open — and not the second: which base each pull request *should* have is a
/// question about the local chain of changes, which this command does not read
/// and `jj spr diff` does. So the branch stays where it was put, the pull
/// request is open on it and shows what it adds to the master branch, and the
/// line below says what to run to finish the job.
///
/// The branch goes back at the tip of the master branch. Any commit reopens a
/// pull request, since a resurrected base ref is checked for existence and not
/// identity, and that one is the truthful choice here: the branch went away
/// because what was on it landed, so what the pull request adds to the master
/// branch is what it adds to what its base became.
async fn put_back_closed_pull_requests(
    opts: &CleanupOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let closed = gh.get_closed_pull_requests().await?;

    // Ours to repair, and cheap to decide before asking the remote anything:
    // a pull request whose head branch this jj-spr did not push is somebody
    // else's, whatever closed it.
    let mine = closed
        .into_iter()
        .filter(|pull_request| config.is_spr_branch(pull_request.head.branch_name()))
        .collect::<Vec<_>>();

    if mine.is_empty() {
        return Ok(());
    }

    let mut broken: Vec<ClosedPullRequest> = Vec::new();

    for pull_request in mine {
        // Asked in the same order as `reopen::diagnose`, and for the same
        // reason: of the two branches, the one whose absence cannot be repaired
        // is the one worth finding first.
        if !gh.remote_branch_exists(&pull_request.head).await?
            || gh.remote_branch_exists(&pull_request.base).await?
        {
            continue;
        }

        broken.push(pull_request);
    }

    if broken.is_empty() {
        return Ok(());
    }

    output(
        "🚑",
        &format!(
            "Found {} closed Pull Request(s) whose base branch is gone from the remote:",
            broken.len()
        ),
    )?;

    let term = console::Term::stdout();
    for pull_request in &broken {
        term.write_line(&format!(
            "     {}",
            console::style(format!(
                "#{} (was based on {})",
                pull_request.number,
                pull_request.base.branch_name()
            ))
            .dim()
        ))?;
    }

    if !opts.confirm {
        output(
            "💡",
            "Run with --confirm to put these branches back and reopen them.",
        )?;
        return Ok(());
    }

    let master_oid = jj.resolve_reference(config.master_ref.local())?;

    for pull_request in &broken {
        // The scaffold is kept, not taken away — it is the pull request's base
        // until something retargets it, and taking it away would close the pull
        // request again. `jj spr diff` is what moves the pull request off it and
        // deletes it then.
        let put_back =
            reopen::put_back(gh, pull_request.number, &pull_request.base, master_oid).await;

        match put_back {
            Ok(scaffold) => {
                if let Some(scaffold) = scaffold {
                    scaffold.keep();
                }

                output(
                    "✅",
                    &format!(
                        "Reopened Pull Request #{} and put {} back",
                        pull_request.number,
                        pull_request.base.branch_name()
                    ),
                )?;
            }
            Err(error) => {
                output(
                    "⚠️",
                    &format!(
                        "Could not reopen Pull Request #{}: {}",
                        pull_request.number, error
                    ),
                )?;
            }
        }
    }

    output(
        "💡",
        "Run `jj spr diff` over these changes to put their bases back where they \
         belong and take the restored branches away.",
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_spr_branch_names_filters_by_prefix() {
        let refs: HashSet<String> = [
            "refs/remotes/origin/spr/user/my-feature",
            "refs/remotes/origin/spr/user/main.my-feature",
            "refs/remotes/origin/main",
            "refs/remotes/origin/other-branch",
            "refs/heads/local-branch",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let mut branches = extract_spr_branch_names(&refs, "origin", "spr/user/");
        branches.sort();

        assert_eq!(
            branches,
            vec!["spr/user/main.my-feature", "spr/user/my-feature"]
        );
    }

    #[test]
    fn test_extract_spr_branch_names_empty_when_no_match() {
        let refs: HashSet<String> = ["refs/remotes/origin/main", "refs/remotes/origin/feature"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let branches = extract_spr_branch_names(&refs, "origin", "spr/user/");
        assert!(branches.is_empty());
    }

    #[test]
    fn test_extract_spr_branch_names_respects_remote_name() {
        let refs: HashSet<String> = [
            "refs/remotes/origin/spr/user/feat",
            "refs/remotes/upstream/spr/user/feat",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let branches = extract_spr_branch_names(&refs, "upstream", "spr/user/");
        assert_eq!(branches, vec!["spr/user/feat"]);
    }

    #[test]
    fn test_find_orphan_branches_identifies_orphans() {
        let spr_branches: Vec<String> = vec![
            "spr/user/feat-a".into(),
            "spr/user/main.feat-a".into(),
            "spr/user/feat-b".into(),
            "spr/user/feat-c".into(),
        ];

        let open_pr_branches: HashSet<String> = ["spr/user/feat-a", "spr/user/main.feat-a"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut orphans: Vec<&str> = find_orphan_branches(&spr_branches, &open_pr_branches)
            .into_iter()
            .map(|s| s.as_str())
            .collect();
        orphans.sort();

        assert_eq!(orphans, vec!["spr/user/feat-b", "spr/user/feat-c"]);
    }

    #[test]
    fn test_find_orphan_branches_none_when_all_active() {
        let spr_branches: Vec<String> =
            vec!["spr/user/feat-a".into(), "spr/user/main.feat-a".into()];

        let open_pr_branches: HashSet<String> = ["spr/user/feat-a", "spr/user/main.feat-a"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let orphans = find_orphan_branches(&spr_branches, &open_pr_branches);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_find_orphan_branches_all_orphans_when_no_open_prs() {
        let spr_branches: Vec<String> = vec!["spr/user/feat-a".into(), "spr/user/feat-b".into()];

        let open_pr_branches: HashSet<String> = HashSet::new();

        let orphans = find_orphan_branches(&spr_branches, &open_pr_branches);
        assert_eq!(orphans.len(), 2);
    }
}
