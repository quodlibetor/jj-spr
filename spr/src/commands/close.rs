/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;

use crate::{
    error::{Error, Result, add_error},
    github::{PullRequestState, PullRequestUpdate},
    jj::PreparedCommit,
    message::MessageSection,
    native_stacks::{
        DissolveReason, StackSession, dissolve_stacks_holding, left_unstacked_by_a_close,
    },
    output::{output, write_commit_title},
    stacked::{
        base_branch_is_ours, may_delete_base_branch, retarget_stacked_pull_requests,
        spawn_branch_deletion, spawn_head_branch_deletion,
    },
};

#[derive(Debug, clap::Parser)]
pub struct CloseOptions {
    /// Close Pull Requests for commits in range from base to revision
    #[clap(long, short = 'a')]
    all: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,
}

pub async fn close(
    opts: CloseOptions,
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
) -> Result<()> {
    let mut result = Ok(());

    // Determine revision and whether to use range mode
    let (use_range_mode, base_rev, target_rev, is_inclusive) =
        crate::revision_utils::parse_revision_and_range(
            opts.revision.as_deref(),
            opts.all,
            opts.base.as_deref(),
        )?;

    let mut prepared_commits = if use_range_mode {
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };

    if prepared_commits.is_empty() {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return result;
    }

    // One session for the whole run rather than one per pull request, so that
    // `--all` pays for a stack once. Dissolving releases every member, and the
    // session remembers that, so closing the rest of a stack asks GitHub nothing
    // further — and what the run left loose is one list at the end rather than a
    // sentence per change, the first of which would name Pull Requests that
    // later iterations go on to close.
    let mut stacks = config
        .stack_display
        .draws_the_stack()
        .then(StackSession::new);

    for prepared_commit in prepared_commits.iter_mut() {
        if result.is_err() {
            break;
        }

        write_commit_title(prepared_commit)?;

        // The further implementation of the close command is in a separate function.
        // This makes it easier to run the code to update the local commit message
        // with all the changes that the implementation makes at the end, even if
        // the implementation encounters an error or exits early.
        result = close_impl(jj, gh, config, prepared_commit, stacks.as_mut()).await;
    }

    // What the stacks this run took apart were holding, less what went back into
    // one — which is nothing, since only `diff` registers a stack. Reported on
    // the way out however the run went: a close that failed part-way through has
    // still freed whatever it freed, and a dissolved stack leaves no record to
    // find its members from afterwards.
    if let Some(sentence) = stacks
        .as_ref()
        .and_then(|session| left_unstacked_by_a_close(&session.orphaned()))
    {
        add_error(&mut result, output("🧱", &sentence));
    }

    // This updates the commit message in the local Jujutsu repository (if it was
    // changed by the implementation)
    add_error(
        &mut result,
        jj.rewrite_commit_messages(&mut prepared_commits),
    );

    result
}

async fn close_impl(
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    prepared_commit: &mut PreparedCommit,
    mut stacks: Option<&mut StackSession>,
) -> Result<()> {
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

    output("📖", "Getting started...")?;

    // Look up the Pull Requests based on this one's head branch before anything
    // changes on GitHub, so that a failure here stops a close that can still be
    // retried. See [`crate::stacked`] for why GitHub is asked rather than the
    // local stack.
    let stacked_pull_requests = gh.get_pull_requests_with_base(&pull_request.head).await?;

    let result = gh
        .update_pull_request(
            pull_request_number,
            PullRequestUpdate {
                state: Some(PullRequestState::Closed),
                ..Default::default()
            },
        )
        .await;

    match result {
        Ok(()) => (),
        Err(error) => {
            output("❌", "GitHub Pull Request close failed")?;

            return Err(error);
        }
    };

    output("📕", "Closed!")?;

    // Closing does not take this Pull Request out of its stack, so the dissolve
    // below counts it among what that stack was holding. It is finished with,
    // and this is what keeps it out of the report; see `StackSession::closed`.
    // Recorded here rather than after the dissolve because it is true either
    // way, and several steps below can leave this function with `?`.
    if let Some(session) = stacks.as_deref_mut() {
        session.closed(pull_request_number);
    }

    // Remove sections from commit that are not relevant after closing.
    prepared_commit.message.remove(&MessageSection::PullRequest);
    prepared_commit.message.remove(&MessageSection::ReviewedBy);
    prepared_commit.message_changed = true;

    // Both halves of this differ from `land`, which dissolves the stack holding
    // the Pull Request it is landing unconditionally — whether or not anything
    // is stacked on it, because GitHub refuses to merge a Pull Request a stack
    // holds at all. Dissolving is not undoable and only `jj spr diff` puts a
    // stack back, so neither difference is small.
    //
    // **Which stacks**: the ones holding the Pull Requests being moved. Holding
    // the Pull Request being *closed* is not itself a reason — closing sends no
    // base, and GitHub allows it while a stack holds the Pull Request. Usually
    // that is one and the same stack and it comes apart anyway; what differs is
    // a close with nothing stacked on it, which dissolves nothing at all. The
    // closed Pull Request then stays in its stack, in place, with the stack
    // still open, and GitHub goes on drawing it there. Deleting its head branch
    // further down does not disturb that stack either — GitHub goes on listing
    // it, open, with the closed member and its vanished head ref (closed and
    // deleted against a live stack on 2026-07-31, and the stack read back
    // afterwards). There is no way to take one Pull Request out of a stack
    // anyway, so the alternative would be destroying it for every other member
    // to tidy up this one. The cost is that a *later* run which dissolves that
    // stack counts the closed member among what it freed; see
    // `native_stacks::unmerged_members`.
    //
    // **Where**: after the close rather than before it. Everything above this
    // point can fail on a close that is still worth retrying — the lookups, and
    // the close itself, which is the likeliest of them — and a close GitHub
    // refuses must not have cost a stack.
    dissolve_stacks_holding(
        stacks,
        gh,
        &stacked_pull_requests,
        DissolveReason::ToFollowAClosing,
    )
    .await?;

    // Closing puts nothing on the master branch, so the Pull Requests that were
    // based on this one belong on *its* base rather than on master: that way
    // their diffs absorb the changes of this Pull Request alone, and not those
    // of every Pull Request below it, which are still under review. When this
    // one was at the bottom of the stack its base is the master branch anyway.
    let retargeted = retarget_stacked_pull_requests(
        gh,
        &stacked_pull_requests,
        &pull_request.base,
        &pull_request.head,
    )
    .await?;

    // Those Pull Requests now show this closed one's changes as part of theirs.
    // That is the point — the changes are no longer under review anywhere else
    // — but it is not something to let happen quietly.
    for number in &retargeted.moved {
        output(
            "📄",
            &format!(
                "Pull Request #{number} is based on {} now, so its diff also \
                 contains the changes of the closed Pull Request \
                 #{pull_request_number}",
                pull_request.base.branch_name()
            ),
        )?;
    }

    // What is based on the base branch now that the retargeting is done —
    // which, where it went well, is the Pull Requests just moved onto it.
    // GitHub is asked rather than that list assumed, because nothing says this
    // Pull Request is the only one that was ever based on the branch, and a
    // second one is what deleting it would close. Only asked where the answer
    // could change anything: under a linear `spr.baseStrategy` the base is the
    // head branch of the Pull Request below and stays either way.
    //
    // Asking can fail, and that must not fail the close: by this point the Pull
    // Request is closed and the local message has lost its number, so an error
    // here would leave nothing to retry with and, in `--all` mode, stop the
    // Pull Requests above from being closed at all. A lookup that did not come
    // back is reported and keeps the branch, which is what not knowing what a
    // deletion would close should cost.
    let based_on_base_branch = if !base_branch_is_ours(config, &pull_request.base) {
        None
    } else {
        match gh.get_pull_requests_with_base(&pull_request.base).await {
            Ok(pull_requests) => Some(pull_requests),
            Err(error) => {
                output(
                    "⚠️",
                    &format!(
                        "Could not find out what is based on {}, so keeping it",
                        pull_request.base.branch_name()
                    ),
                )?;
                for message in error.messages() {
                    output("  ", message)?;
                }

                None
            }
        }
    };

    let remove_old_branch_child_process =
        spawn_head_branch_deletion(jj, config, &pull_request.head, &retargeted)?;

    let remove_old_base_branch_child_process =
        if may_delete_base_branch(config, &pull_request.base, based_on_base_branch.as_deref()) {
            Some(spawn_branch_deletion(jj, config, &pull_request.base)?)
        } else {
            None
        };

    // Wait for the "git push" to delete the old Pull Request branch to finish,
    // but ignore the result.
    // GitHub may be configured to delete the branch automatically,
    // in which case it's gone already and this command fails.
    if let Some(mut proc) = remove_old_branch_child_process {
        proc.wait().await?;
    }
    if let Some(mut proc) = remove_old_base_branch_child_process {
        proc.wait().await?;
    }

    Ok(())
}
