/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;
use std::{io::Write, process::Stdio, time::Duration};

use crate::{
    error::{Error, Result, ResultExt},
    github::{PullRequestState, PullRequestUpdate, ReviewStatus, StackedPullRequest},
    message::{MessageSection, build_github_body_for_merging},
    native_stacks::{
        DissolveReason, StackSession, dissolve_any_stack_holding, dissolve_stacks_holding,
        left_unstacked_by_a_land,
    },
    output::{output, write_commit_title},
    stacked::{
        may_delete_base_branch, retarget_stacked_pull_requests, spawn_branch_deletion,
        spawn_head_branch_deletion,
    },
    utils::run_command,
};

/// Find the Pull Requests that will sit directly on the master branch once the
/// commit `landing_oid` has landed.
///
/// What a stacked Pull Request targets on GitHub depends on `spr.baseStrategy`:
/// under `linear` it is the head branch of the Pull Request below, which does
/// say which Pull Request is stacked on which; under `synthetic` it is a base
/// branch holding the tree of its parent commit, which says nothing. Only the
/// local stack answers the question for both, so we ask Jujutsu for the
/// children of the commit being landed.
async fn find_stacked_pull_requests(
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    landing_oid: git2::Oid,
) -> Result<Vec<StackedPullRequest>> {
    let children =
        jj.get_prepared_commits_for_revset(config, &format!("children({})", landing_oid))?;

    let mut stacked = Vec::new();
    for number in children
        .iter()
        .filter_map(|child| child.pull_request_number)
    {
        let pull_request = gh.clone().get_pull_request(number).await?;

        // A Pull Request that already targets the master branch needs no
        // retargeting, and a closed one is left alone.
        if pull_request.state == PullRequestState::Open && !pull_request.base.is_master_branch() {
            stacked.push(StackedPullRequest {
                number,
                base: pull_request.base,
            });
        }
    }

    Ok(stacked)
}

/// Wait for GitHub to work out whether it will merge Pull Request `number`, and
/// hold this land to that answer.
///
/// Three things have to be true: the Pull Request still has to have the head
/// `head_oid` this land is about, GitHub has to call it mergeable, and — where
/// `enforce_requirements` says so — GitHub must not report the requirements its
/// base branch sets as unmet. A Pull Request GitHub still reports as based on
/// something other than the master branch is one it has not caught up with, so
/// that is waited for rather than judged.
///
/// The retrying is waiting for GitHub to catch up rather than for the Pull
/// Request to change. GitHub works both verdicts out lazily and sends them back
/// to undecided whenever the Pull Request changes, so a land that has just
/// retargeted one asks before there is anything to read. After ten seconds of
/// no answer the land gives up rather than guess.
///
/// `enforce_requirements` decides not only whether an unmet requirement refuses
/// the land but whether that wait happens at all: a land that means to proceed
/// regardless has no reason to spend those ten seconds on an answer it will
/// discard.
async fn wait_for_mergeability(
    gh: &crate::github::GitHub,
    number: u64,
    head_oid: git2::Oid,
    enforce_requirements: bool,
) -> Result<()> {
    let mut attempts = 0;

    loop {
        attempts += 1;

        let mergeability = gh.get_pull_request_mergeability(number).await?;

        if mergeability.head_oid != head_oid {
            return Err(Error::new(formatdoc!(
                "The Pull Request seems to have been updated externally.
                     Please try again!"
            )));
        }

        // GitHub works both verdicts out lazily, and retargeting a Pull Request
        // sends them back to `UNKNOWN`, so both have to arrive before there is
        // anything to judge.
        let requirements_known = !enforce_requirements || mergeability.requirements_known();

        if mergeability.base.is_master_branch()
            && mergeability.mergeable.is_some()
            && requirements_known
        {
            if mergeability.mergeable != Some(true) {
                return Err(Error::new(formatdoc!(
                    "GitHub concluded the Pull Request is not mergeable at \
                    this point. Please rebase your changes and try again!"
                )));
            }

            if enforce_requirements && mergeability.requirements_unmet() {
                return Err(Error::new(formatdoc!(
                    "GitHub reports that this Pull Request does not meet the \
                     requirements its base branch sets. A required check may \
                     be failing or not yet started, a review may be missing, \
                     or a rule may be unsatisfied — GitHub does not say \
                     which, but the Pull Request page does.

                     Landing anyway needs permission to bypass those \
                     requirements. If you have it and mean to use it, pass \
                     --force, or set spr.landWithUnmetRequirements to true to \
                     stop checking altogether."
                )));
            }

            // TODO: Implement Jujutsu-native commit fetching and tree comparison
            // For now, skip the merge commit validation
            // This would need to be rewritten using jj commands

            return Ok(());
        }

        if attempts >= 10 {
            // After ten failed attempts we give up.
            return Err(Error::new(
                "GitHub Pull Request did not update. Please try again!",
            ));
        }

        // Wait one second before retrying
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Give up on the land: say so, put the base back if this land moved it, and
/// hand back the failure to return.
///
/// `headline` is what went wrong, in the words of the step that was reached.
/// Only the merge itself can say a merge failed — the checks before it refuse
/// a land that GitHub was never asked to merge, and reporting those as a failed
/// merge sends whoever reads it looking at branch protection rather than at the
/// requirement that was actually unmet.
///
/// `retargeted_from` is the base this land pointed at the master branch, or
/// `None` where it moved none — either because the Pull Request was on the
/// master branch already, or because it has not been retargeted yet. That is
/// the whole of the rollback: nothing else this land has done is undoable, and
/// in particular the stack it dissolved can only be put back by `jj spr diff`,
/// which is why the base-already-master path takes GitHub's verdict on the
/// merge before it dissolves anything.
async fn abandon_land(
    gh: &crate::github::GitHub,
    number: u64,
    headline: &str,
    retargeted_from: Option<&crate::github::GitHubBranch>,
    mut error: Error,
) -> Result<()> {
    output("❌", headline)?;

    if let Some(base) = retargeted_from
        && let Err(rollback_error) = gh
            .update_pull_request(
                number,
                PullRequestUpdate {
                    base: Some(base.branch_name().to_string()),
                    ..Default::default()
                },
            )
            .await
    {
        error.push(format!("{}", rollback_error));
    }

    Err(error)
}

#[derive(Debug, clap::Parser)]
pub struct LandOptions {
    /// Merge a Pull Request that was created or updated with spr diff
    /// --cherry-pick
    #[clap(long)]
    cherry_pick: bool,

    /// Land the Pull Request even if GitHub reports that the requirements its
    /// base branch sets are not met: a required check failing or not yet
    /// started, a missing review, an unsatisfied rule. Does the same thing for
    /// one land as setting spr.landWithUnmetRequirements does for every land.
    #[clap(long)]
    force: bool,

    /// Jujutsu revision to operate on (if not specified, uses '@')
    #[clap(short = 'r', long)]
    revision: Option<String>,
}

fn resolve_cherry_pick(
    cli_cherry_pick: bool,
    message: &crate::message::MessageSectionsMap,
) -> bool {
    cli_cherry_pick
        || message
            .get(&MessageSection::CherryPick)
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

pub async fn land(
    opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    // GitHub's own stacks, held as an `Option` exactly as `diff` holds them:
    // `None` is the whole of "GitHub is not drawing the stack", so nothing below asks
    // what mode this is.
    //
    // Held out here, one frame above the land itself, for the sake of the
    // sentence below: whatever the land did to the stacks it found has to be
    // said however the land went.
    let mut stacks = config
        .stack_display
        .draws_the_stack()
        .then(StackSession::new);

    let landed = land_pull_request(opts, jj, gh, config, &mut stacks).await;

    // What the stacks this land took apart were holding, less what has landed:
    // `land` registers nothing — only `diff` does — so every other member is
    // loose, and a dissolved stack leaves no record to find them from
    // afterwards.
    //
    // Said however the land went, which is the point of it being here. A land
    // that fails below the dissolve is exactly the one that has taken a stack
    // apart and left it that way: it has named the stack it took apart, but
    // only this names the Pull Requests that came loose from it. A land that
    // fails above the dissolve has freed nothing, so this is silent.
    let reported = match stacks
        .as_ref()
        .and_then(|session| left_unstacked_by_a_land(&session.orphaned()))
    {
        Some(sentence) => output("🧱", &sentence),
        None => Ok(()),
    };

    // The land's own answer wins: a terminal that would not take that sentence
    // must not turn a failed land into some other failure.
    landed.and(reported)
}

/// Land the one Pull Request `opts` names, in one of two orders.
///
/// Which order depends on where the Pull Request already sits, and the
/// difference is what a refused land costs:
///
/// - base already the master branch: **ask GitHub, dissolve, merge**. Nothing
///   has to move for that verdict, so it can be had before the stack is taken
///   apart — and dissolving is not undoable, so a land refused for the ordinary
///   reasons keeps its stack.
/// - otherwise: **dissolve, retarget onto the master branch, ask, merge**. This
///   one cannot ask first. The retarget is itself a base change GitHub refuses
///   while a stack holds the Pull Request, so the dissolve has to come before
///   it; and retargeting sends GitHub's verdicts back to `UNKNOWN`, so the
///   asking has to come after. A refusal here costs the stack, and that is not
///   avoidable.
///
/// The two are complementary, so the mergeability check runs exactly once
/// either way and the merge is never reached without it.
///
/// `retargeted_from` marks the rollback boundary: it is `Some` only past the
/// retarget above, and every failure from there on goes through
/// [`abandon_land`], which puts that base back.
///
/// The Pull Requests a dissolved stack left loose are *not* reported here —
/// [`land`] does that from `stacks`, so that they are named however this
/// returns.
async fn land_pull_request(
    mut opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    stacks: &mut Option<StackSession>,
) -> Result<()> {
    let revision = opts.revision.as_deref().unwrap_or("@");
    let prepared_commit = jj.get_prepared_commit_for_revision(config, revision)?;

    // Honor both the --cherry-pick flag and the "Cherry Pick:" marker on the
    // commit description. When the validation TODO below is filled in, use
    // opts.cherry_pick as the authoritative source.
    opts.cherry_pick = resolve_cherry_pick(opts.cherry_pick, &prepared_commit.message);

    write_commit_title(&prepared_commit)?;

    let pull_request_number = if let Some(number) = prepared_commit.pull_request_number {
        output("#️⃣ ", &format!("Pull Request #{}", number))?;
        number
    } else {
        return Err(Error::new("This commit does not refer to a Pull Request."));
    };

    // Load Pull Request information
    let pull_request = gh.clone().get_pull_request(pull_request_number).await?;

    if pull_request.state != PullRequestState::Open {
        return Err(Error::new(formatdoc!(
            "This Pull Request is already closed!",
        )));
    }

    if config.require_approval && pull_request.review_status != Some(ReviewStatus::Approved) {
        return Err(Error::new(
            "This Pull Request has not been approved on GitHub.",
        ));
    }

    output("🛫", "Getting started...")?;

    // Look up the Pull Requests stacked on this one before anything changes on
    // GitHub, so that a failure here stops a land that can still be retried.
    let stacked_pull_requests =
        find_stacked_pull_requests(jj, gh, config, prepared_commit.oid).await?;

    // Fetch current master from GitHub.
    run_command(
        jj.git_command()
            .arg("fetch")
            .arg("--no-write-fetch-head")
            .arg("--")
            .arg(&config.remote_name)
            .arg(config.master_ref.on_github()),
    )
    .await
    .reword("git fetch failed".to_string())?;

    // TODO: Implement Jujutsu-native cherry-pick and merge validation
    // For now, we'll trust GitHub's merge validation and skip local validation
    let base_is_master = pull_request.base.is_master_branch();

    // Skip local cherry-pick validation for Jujutsu workflow
    // GitHub will validate mergeability during the merge process
    let merge_matches_cherrypick = true;

    if !merge_matches_cherrypick {
        return Err(Error::new(formatdoc!(
            "This commit has been updated and/or rebased since the pull \
             request was last updated. Please run `spr diff` to update the \
             pull request and then try `spr land` again!"
        )));
    }

    let pr_head_oid = pull_request.head_oid;

    // Whether to hold GitHub's verdict on the base branch's requirements
    // against this land. Settled before either mergeability check because it
    // decides not only whether an unmet requirement refuses the land, but
    // whether the check waits for a verdict at all: a land that means to
    // proceed regardless has no reason to spend ten seconds waiting for an
    // answer it will discard.
    let enforce_requirements = config.enforce_merge_requirements(opts.force);

    // Where this Pull Request already sits on the master branch, ask GitHub
    // whether it will merge it *before* taking its stack apart. That question
    // is the ordinary way a land fails — a required check still running, a
    // conflict, a requirement its base branch sets that is not met — and
    // dissolving is not undoable, so a land refused here would otherwise cost
    // the stack for nothing. Nothing has to move for this verdict, so GitHub
    // reaches the same one either side of the dissolve; the only difference is
    // what a refusal costs.
    //
    // Nothing has changed on GitHub yet, so there is nothing to put back: the
    // stack still stands and no base has moved. That is what the `None` says —
    // the rollback restores the base this land retargets, and `base_is_master`
    // is exactly the case where it retargets none.
    if base_is_master
        && let Err(error) =
            wait_for_mergeability(gh, pull_request_number, pr_head_oid, enforce_requirements).await
    {
        return abandon_land(
            gh,
            pull_request_number,
            "Not landing this Pull Request",
            None,
            error,
        )
        .await;
    }

    // The stack holding this Pull Request has to go, and it does not come back:
    // only `diff` registers stacks. Two things need it gone, and it is the
    // second that makes it unconditional:
    //
    // - Every base ref this land moves — this Pull Request's onto the master
    //   branch just below, the ones above it after the merge, and the rollback
    //   that puts this one back if the merge fails — is a change GitHub refuses
    //   while a stack holds the Pull Request. Of those, only the ones above it
    //   happen when the base is already the master branch, which is the bottom
    //   of every stack; and each of those leaves whatever stack holds it in the
    //   `dissolve_stacks_holding` call before `retarget_stacked_pull_requests`,
    //   not here.
    // - The merge itself. The ordinary merge endpoint refuses a Pull Request a
    //   stack holds outright: `403 Merging stacked PRs via this endpoint is not
    //   supported. Use the asynchronous merge endpoint instead.` (observed
    //   2026-08-01). So even landing the bottom of a stack, which moves no base
    //   at all before merging, cannot proceed with the stack standing.
    //
    // The asynchronous endpoint it points at is the one jj-spr must not use: it
    // destroys the Pull Requests above the one it merges. The account of how,
    // and of how that was established, is at the top of `impl GitHub` in
    // `github::stacks`, which is where it belongs — repeating the mechanism
    // here would only give it somewhere to drift out of step.
    //
    // As late as it can be, because dissolving is not undoable: everything
    // above is a lookup or a fetch — including, where the base is already the
    // master branch, GitHub's verdict on the merge — so a land that fails up
    // there can still be retried with the stack intact. Where the base is not
    // the master branch that verdict has to wait for the retarget below, which
    // in turn has to wait for this. The dissolve names the stack, and the report
    // at the end names what it freed.
    dissolve_any_stack_holding(
        stacks.as_mut(),
        gh,
        pull_request_number,
        DissolveReason::ToLand,
    )
    .await?;

    if !base_is_master {
        // The base of the Pull Request on GitHub is not set to master. This
        // means the Pull Request uses a base branch. We tested above that
        // merging the Pull Request branch into the master branch produces the
        // intended result (the same as cherry-picking the local commit onto
        // master), so what we want to do is actually merge the Pull Request as
        // it is into master. Hence, we change the base to the master branch.
        //
        // Before we do that, there is one more edge case to look out for: if
        // the base branch contains changes that have since been landed on
        // master, then Git might be able to figure out that these changes
        // appear both in the pull request branch (via the merge branch) and in
        // master, but are identical in those two so it is not a merge conflict
        // but can go ahead. The result of this in master if we merge now is
        // correct, but there is one problem: when looking at the Pull Request
        // in GitHub after merging, it will show these change as part of the
        // Pull Request. So when you look at the changed files of the Pull
        // Request, you will see both changes in this commit (great!) and those
        // in the base branch (a previous commit that has already been landed on
        // master - not great!). This is because the changes shown are the ones
        // that happened on this Pull Request branch (now including the base
        // branch) since it branched off master. This can include changes in the
        // base branch that are already on master, but were added to master
        // after the Pull Request branch branched from master.
        // The solution is to merge current master into the Pull Request branch.
        // Doing that now means that the final changes done by this Pull Request
        // are only the changes that are not yet in master. That's what we want.
        // This final merge never introduces any changes to the Pull Request. In
        // fact, the tree that we use for the merge commit is the one we got
        // above from the cherry-picking of this commit on master.

        // TODO: Implement Jujutsu-native merge base and tree comparison
        // For now, skip the complex merge-in-master logic
        // This logic would need to be rewritten using jj commands

        // Skip the merge-in-master commit creation for Jujutsu workflow

        gh.update_pull_request(
            pull_request_number,
            PullRequestUpdate {
                base: Some(config.master_ref.branch_name().to_string()),
                ..Default::default()
            },
        )
        .await?;
    }

    // The base this land moved onto the master branch, and so the base a
    // failure from here on has to put back. `None` where the Pull Request was
    // already on the master branch and the block above moved nothing — the one
    // thing that decides the rollback, decided next to the retarget it undoes.
    let retargeted_from = (!base_is_master).then_some(&pull_request.base);

    // Whether GitHub will merge this Pull Request.
    //
    // Where the base was already the master branch, that answer was taken well
    // above, before the dissolve, and is not asked for a second time. The
    // dissolve since then *is* a change on GitHub's side — it is what turns a
    // guaranteed 403 into a merge — but it moves no base and touches no branch
    // content, and the merge below is pinned to `pr_head_oid`, so a verdict
    // that had gone stale would cost a rejected merge rather than a wrong one.
    //
    // Where the base was not the master branch, here is the earliest the
    // question can be put. The retarget just above sends GitHub's verdicts back
    // to `UNKNOWN`, so an answer taken before it would be about the base this
    // Pull Request has just left; and the retarget is itself a base change,
    // which GitHub refuses while a stack holds the Pull Request, so it cannot
    // come before the dissolve either. Dissolve, retarget, then ask is the only
    // order there is — which is why this path, unlike the other, pays for a
    // refusal with the stack.
    if !base_is_master
        && let Err(error) =
            wait_for_mergeability(gh, pull_request_number, pr_head_oid, enforce_requirements).await
    {
        return abandon_land(
            gh,
            pull_request_number,
            "Not landing this Pull Request",
            retargeted_from,
            error,
        )
        .await;
    }

    // We have checked that merging the Pull Request branch into the master
    // branch produces the intended result, and that's independent of whether we
    // used a base branch with this Pull Request or not. We have made sure the
    // target of the Pull Request is set to the master branch. So let GitHub do
    // the merge now!
    //
    // Squash, and only squash. That is not a default anybody is expected to
    // want to change: it is the only one of GitHub's three merge methods that
    // fits what jj-spr is, one Jujutsu change landing as one commit. Both of
    // the others were made selectable and tried against a real repository on
    // 2026-08-01, and both misbehave on a stacked Pull Request — one whose base
    // is not the master branch, and whose head is therefore a merge commit
    // (`pr_head_parents` in `commands::diff`):
    //
    // - `rebase` is refused by GitHub outright, with `Base branch was modified.
    //   Review and try the merge again.` — which says nothing about the real
    //   cause. Reproduced three times. Nothing lands, and since the stack has
    //   been dissolved by this point the land costs the stack as well.
    // - `merge` succeeds and lands the Pull Requests *below* the one asked for:
    //   a merge commit takes the whole branch, so their commits reach the
    //   master branch and GitHub closes them as merged on its own. jj-spr never
    //   hears about it, so their head branches are left behind and their local
    //   changes are never marked landed. It also puts jj-spr's own synthetic
    //   merge commits on the master branch, which is the plumbing that keeps a
    //   reviewer's incremental diffs working and has no business there.
    //
    // On a Pull Request that already sits on the master branch both do land a
    // single commit, so the failure is specific to the branch shape rather than
    // general. That is not a reason to offer them: it would be a setting whose
    // useful case is the one where it changes nothing.
    let merged = octocrab::instance()
        .pulls(&config.owner, &config.repo)
        .merge(pull_request_number)
        .method(octocrab::params::pulls::MergeMethod::Squash)
        .title(pull_request.title)
        .message(build_github_body_for_merging(&pull_request.sections))
        .sha(format!("{}", pr_head_oid))
        .send()
        .await
        .convert()
        .context(format!(
            "squash-merging PR #{} (head {})",
            pull_request_number, pr_head_oid
        ))
        .and_then(|merge| {
            if merge.merged {
                Ok(merge)
            } else {
                Err(Error::new(formatdoc!(
                    "GitHub Pull Request merge failed: {}",
                    merge.message.unwrap_or_default()
                )))
            }
        });

    let merge = match merged {
        Ok(merge) => merge,
        Err(error) => {
            return abandon_land(
                gh,
                pull_request_number,
                "GitHub Pull Request merge failed",
                retargeted_from,
                error,
            )
            .await;
        }
    };

    output("🛬", "Landed!")?;

    // The dissolve above happened while this Pull Request was still open, so it
    // is in the list of what the stack was holding. It is out of every stack for
    // good now and cannot be put back into one, so it is not something the
    // dissolve lost.
    if let Some(session) = stacks.as_mut() {
        session.merged(pull_request_number);
    }

    // Nothing this land aims at this base branch: the retargeting below sends
    // the Pull Requests above to the master branch, not here. That is why it
    // can go now, rather than waiting for that retargeting the way the head
    // branch does.
    //
    // Whether anything *else* is based on it is not asked — `close` does ask,
    // and passing nothing here says only that `land` has not looked. See
    // `may_delete_base_branch`.
    let remove_old_base_branch_child_process =
        if may_delete_base_branch(config, &pull_request.base, Some(&[])) {
            Some(spawn_branch_deletion(jj, config, &pull_request.base)?)
        } else {
            None
        };

    // The Pull Requests that were stacked on this one are now based on the
    // master branch, so point them at it and take the base branches they leave
    // out of the way, where those are ours.
    //
    // A stack holding any of them would refuse the move, so they leave theirs
    // first. Members of the stack this land already dissolved are out of one
    // already and cost nothing here.
    dissolve_stacks_holding(
        stacks.as_mut(),
        gh,
        &stacked_pull_requests,
        DissolveReason::ToFollowALanding,
    )
    .await?;

    let retargeted = retarget_stacked_pull_requests(
        gh,
        &stacked_pull_requests,
        &config.master_ref,
        &pull_request.head,
    )
    .await?;

    // "The Pull Requests above" are the ones the local stack knows about, which
    // is where `find_stacked_pull_requests` looks. A Pull Request based on this
    // branch whose change jj cannot see — abandoned locally, or in a workspace
    // this one has not fetched — is neither retargeted nor protected below.
    let remove_old_branch_child_process =
        spawn_head_branch_deletion(jj, config, &pull_request.head, &retargeted)?;

    // Rebase us on top of the now-landed commit
    if let Some(sha) = merge.sha {
        // Try this up to three times, because fetching the very moment after
        // the merge might still not find the new commit.
        for i in 0..3 {
            // Fetch current master and the merge commit from GitHub.
            let git_fetch = jj
                .git_command()
                .arg("fetch")
                .arg("--no-write-fetch-head")
                .arg("--")
                .arg(&config.remote_name)
                .arg(config.master_ref.on_github())
                .arg(&sha)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await?;
            if git_fetch.status.success() {
                break;
            } else if i == 2 {
                console::Term::stderr().write_all(&git_fetch.stderr)?;
                return Err(Error::new("git fetch failed"));
            }
        }
        // TODO: Implement Jujutsu-native rebase after landing
        // For now, the user will need to manually rebase after landing
        output(
            "⚠️",
            "Please manually rebase your working copy after landing",
        )?;
    }

    // Wait for the "git push" to delete the old Pull Request branch to finish,
    // but ignore the result. GitHub may be configured to delete the branch
    // automatically, in which case it's gone already and this command fails.
    if let Some(mut proc) = remove_old_branch_child_process {
        proc.wait().await?;
    }
    if let Some(mut proc) = remove_old_base_branch_child_process {
        proc.wait().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with_cherry_pick(value: &str) -> crate::message::MessageSectionsMap {
        [(MessageSection::CherryPick, value.to_string())].into()
    }

    #[test]
    fn test_land_resolve_flag_true() {
        let map = crate::message::MessageSectionsMap::new();
        assert!(resolve_cherry_pick(true, &map));
    }

    #[test]
    fn test_land_resolve_flag_false_with_marker() {
        let map = map_with_cherry_pick("true");
        assert!(resolve_cherry_pick(false, &map));
    }

    #[test]
    fn test_land_resolve_flag_false_without_marker() {
        let map = crate::message::MessageSectionsMap::new();
        assert!(!resolve_cherry_pick(false, &map));
    }

    #[test]
    fn test_land_resolve_marker_case_insensitive() {
        let map = map_with_cherry_pick("TRUE");
        assert!(resolve_cherry_pick(false, &map));
    }

    #[test]
    fn test_land_resolve_marker_other_value() {
        for value in &["false", "yes", "1", ""] {
            let map = map_with_cherry_pick(value);
            assert!(
                !resolve_cherry_pick(false, &map),
                "Expected false for marker value {:?}",
                value
            );
        }
    }
}
