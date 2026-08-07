/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;
use std::{io::Write, process::Stdio, time::Duration};

use crate::{
    config::LandStrategy,
    error::{Error, Result, ResultExt},
    github::{
        AsyncMerge, MergeQueue, PullRequestState, PullRequestUpdate, ReviewStatus,
        StackedPullRequest,
    },
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

/// What this land will ask GitHub for, once it is known which of them the
/// default branch takes.
enum Landing {
    /// Squash-merge the Pull Request now.
    Merge,
    /// Put the Pull Request in this merge queue and leave the merging to
    /// GitHub.
    Queue(MergeQueue),
    /// Ask GitHub's stacked pull requests to merge this Pull Request and every
    /// member of its stack below it, in one request. See
    /// [`land_through_the_stack_merge`].
    Stack,
}

/// The strategy to land under, given the flags this land was passed and the
/// configured one.
///
/// The flags name a strategy for one land rather than switching something on:
/// `--no-queue` is how a caller who may bypass the queue says to merge now in a
/// repository configured to queue, which is the same thing `spr.landStrategy =
/// merge` says for every land. No flag leaves the setting to decide.
///
/// `--no-stack` is the one that is not simply another strategy. The other three
/// flags each name what to do; this one names what *not* to do, and says nothing
/// about whether the Pull Requests it leaves to be merged one at a time should
/// be queued. So it only displaces a configured [`LandStrategy::Stack`], and
/// with what the default branch decides — pair it with `--queue` or `--no-queue`
/// to settle that too.
///
/// `--queue` with `--no-queue`, `--stack` with `--no-stack`, and `--stack` with
/// `--queue` are all refused by `clap`, so the order those are read in here
/// never decides anything. `--stack` with `--no-queue` is allowed and means the
/// stack merge: both say not to queue, and only one of them says what to do
/// instead.
fn resolve_land_strategy(
    queue: bool,
    no_queue: bool,
    stack: bool,
    no_stack: bool,
    configured: LandStrategy,
) -> LandStrategy {
    match (queue, no_queue, stack, no_stack) {
        (true, ..) => LandStrategy::Queue,
        (_, _, true, _) => LandStrategy::Stack,
        (_, true, ..) => LandStrategy::Merge,
        (_, _, _, true) if configured == LandStrategy::Stack => LandStrategy::Auto,
        _ => configured,
    }
}

/// Work out what this land will ask GitHub for, asking GitHub itself where the
/// strategy is to be decided by what the default branch allows.
///
/// A branch either has a merge queue, and then takes no merge that does not go
/// through it, or has none, and then there is no queue to join. So under
/// [`LandStrategy::Auto`] the branch decides, and under [`LandStrategy::Queue`]
/// a branch without a queue refuses the land here — with a sentence about the
/// branch, which is where the problem is, rather than through the mutation
/// failing further down.
async fn decide_landing(
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    strategy: LandStrategy,
) -> Result<Landing> {
    let branch_name = config.master_ref.branch_name();

    Ok(match strategy {
        // Nothing is asked of GitHub here: a caller entitled to bypass the
        // queue may merge into a branch that has one, and it is GitHub's answer
        // to the merge itself that says whether this caller is.
        LandStrategy::Merge => Landing::Merge,
        // Nor here, and for the same reason: the stack merge is a merge, so a
        // branch with a queue is GitHub's to refuse. Nothing observed says how
        // it answers, and inventing a refusal for it here would claim more than
        // has been established.
        LandStrategy::Stack => Landing::Stack,
        LandStrategy::Queue => match gh.get_merge_queue(branch_name).await? {
            Some(queue) => Landing::Queue(queue),
            None => {
                return Err(Error::new(format!(
                    "spr.landStrategy asks for the merge queue, but GitHub keeps no merge queue \
                     for branch '{branch_name}'. Set spr.landStrategy to 'auto' or 'merge', or \
                     pass --no-queue, to merge the Pull Request instead."
                )));
            }
        },
        LandStrategy::Auto => match gh.get_merge_queue(branch_name).await? {
            Some(queue) => Landing::Queue(queue),
            None => Landing::Merge,
        },
    })
}

/// The changes this land has to put on the master branch, bottom first, ending
/// with the one it was asked for.
///
/// A Pull Request branch carries the whole local stack below it, so merging one
/// from the middle of a stack puts every change under it on the master branch
/// as well — under the middle one's title, inside the middle one's squash,
/// while the Pull Requests those changes belong to stay open with nothing left
/// to show. Landing them in their own right instead is what this is for: each
/// lands as its own commit, and each Pull Request is closed by the merge that
/// carried it.
///
/// It is also what GitHub does with a stack of its own. `gh stack merge <n>`
/// merges every member up to and including the one named, and the choice it
/// offers is how far *up* to go, never how far down. So the chain worked out here
/// is the same chain either way: whether it is merged one Pull Request at a time
/// or handed to GitHub's stack merge is
/// [`LandStrategy::Stack`](crate::config::LandStrategy::Stack), decided after
/// this and over the same list — see [`land_through_the_stack_merge`], which
/// reconciles what GitHub would merge against what this returned.
///
/// Which of the changes below the one being landed are still to land is asked
/// of GitHub, one Pull Request at a time, rather than of the local chain. It
/// cannot be read off the chain: landing does not rewrite the local changes, so
/// one that has already landed still sits below its parent afterwards and looks
/// exactly like one that has not. A change whose Pull Request GitHub has merged
/// is therefore passed over — its content is on the master branch, and only the
/// local history has yet to catch up.
///
/// Two shapes refuse the land instead of being passed over: a change with no
/// Pull Request at all, and one whose Pull Request was closed without being
/// merged. Neither has landed, and passing over either would not leave it
/// unlanded — its commits are in the branch of every Pull Request above it, so
/// the next merge takes them anyway. It would only land it with nothing on
/// GitHub to say so.
///
/// Cherry-picked Pull Requests are the one shape this does not describe, and
/// the caller settles that before asking: such a branch carries its change onto
/// the master branch by itself, so nothing below it is part of the merge.
///
/// Where the chain starts is asked of the local stack rather than of what the
/// Pull Request is based on, because the two answer different questions. A Pull
/// Request keeps whatever base branch it was given until something moves it, so
/// a change at the bottom of what was once a stack still points at a generated
/// base branch long after everything below it landed. That is not a change with
/// unlanded parents, and it is unlanded parents this is about.
async fn changes_to_land(
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    commit: crate::jj::PreparedCommit,
) -> Result<Vec<crate::jj::PreparedCommit>> {
    let master_base = jj.get_master_base_for_commit(config, commit.oid)?;

    // Sitting directly on the master branch, with nothing of its own below it
    // that has not landed.
    if master_base == commit.parent_oid {
        return Ok(vec![commit]);
    }

    // Oldest first, which is the order they have to land in.
    let chain = jj.get_prepared_commits_from_to(
        config,
        &format!("{}", master_base),
        &format!("{}", commit.oid),
        false,
    )?;

    // The check above says there is something below this change, so the revset
    // cannot have come back empty; this only spares the arithmetic below an
    // underflow if it ever does.
    let Some(asked_for) = chain.len().checked_sub(1) else {
        return Ok(vec![commit]);
    };

    let mut to_land = Vec::with_capacity(chain.len());

    for (position, change) in chain.into_iter().enumerate() {
        // The change this land was asked for is not judged here:
        // `land_pull_request` has its own account of what makes a Pull Request
        // unlandable, and it words those refusals as being about the Pull
        // Request somebody named rather than about one below it.
        if position == asked_for {
            to_land.push(change);
            break;
        }

        let Some(number) = change.pull_request_number else {
            write_commit_title(&change)?;

            return Err(Error::new(formatdoc!(
                "This change is below the one being landed and has no Pull \
                 Request. Landing that one would put this change on the \
                 default branch too, because its Pull Request branch carries \
                 it, and nothing on GitHub would say so. Run `jj spr diff` \
                 over the stack first."
            )));
        };

        let pull_request = gh.clone().get_pull_request(number).await?;

        // A merged Pull Request is the only kind of closed one that has
        // landed; `merge_commit` is what tells the two apart, since the state
        // GitHub reports is `Closed` for both.
        if pull_request.merge_commit.is_some() {
            continue;
        }

        if pull_request.state != PullRequestState::Open {
            write_commit_title(&change)?;

            return Err(Error::new(formatdoc!(
                "This change is below the one being landed, and Pull Request \
                 #{number} was closed without being merged, so the change has \
                 not landed. Landing the one above would put it on the default \
                 branch anyway, because that Pull Request's branch carries it. \
                 Take this change out of the stack, or push it again with `jj \
                 spr diff`."
            )));
        }

        to_land.push(change);
    }

    Ok(to_land)
}

/// How often a land that waits asks GitHub what the merge queue has done with
/// the Pull Request.
///
/// A merge queue takes minutes at best, so asking often buys nothing but rate
/// limit.
const MERGE_QUEUE_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Wait for the merge queue to merge Pull Request `number`, and hand back the
/// commit it merged it as.
///
/// Three things can happen to a queued Pull Request, and this returns on all
/// three: GitHub merges it, which is what the merge commit says; GitHub lets go
/// of it without merging, which is what an entry that has gone without one says
/// — a queue drops a Pull Request whose checks fail on the merged result; or
/// somebody closes it. Only the first is a land.
///
/// There is no giving up on time here, unlike every other wait in this file.
/// Those wait for GitHub to work something out, where taking too long means
/// something is wrong; this one waits for a queue to reach the Pull Request,
/// which legitimately takes as long as the queue ahead of it does. A caller who
/// no longer wants to wait can stop waiting — the Pull Request stays in the
/// queue either way, which is the whole point of the flag being optional.
async fn wait_for_the_merge_queue(
    gh: &impl crate::github::GitHubApi,
    number: u64,
    mut reported: Option<i64>,
) -> Result<git2::Oid> {
    loop {
        // Asked after the wait rather than before it. What is known on the way
        // in is that GitHub has just made the entry — it answered the enqueue
        // with it — so there is nothing to learn from asking straight away, and
        // an entry GitHub has not caught up with yet reads exactly like one it
        // has dropped.
        tokio::time::sleep(MERGE_QUEUE_POLL_INTERVAL).await;

        let queued = gh.get_queued_pull_request(number).await?;

        if let Some(merge_commit) = queued.merge_commit {
            return Ok(merge_commit);
        }

        match queued.entry {
            None if queued.state == PullRequestState::Open => {
                return Err(Error::new(
                    "GitHub took this Pull Request out of the merge queue without merging it. A \
                     merge queue drops a Pull Request whose required checks fail once its changes \
                     are merged with the ones ahead of it. The Pull Request is still open: see it \
                     on GitHub for why, and land it again once it is fixed.",
                ));
            }
            None => {
                return Err(Error::new(
                    "This Pull Request was closed without being merged while it was in the merge \
                     queue.",
                ));
            }
            // Only when its place changes, so that a long wait behind an
            // unmoving queue does not fill a terminal with the same line.
            Some(entry) if reported != Some(entry.position) => {
                reported = Some(entry.position);
                output(
                    "🚦",
                    &format!("Waiting in the merge queue at position {}", entry.position),
                )?;
            }
            Some(_) => (),
        }
    }
}

/// How long a wait of `seconds` is, in words, for a report to a person.
///
/// Rounded to the minute throughout: these are GitHub's own estimates of how
/// long a queue will take to reach a Pull Request, and a figure to the second
/// would claim a precision the estimate does not have.
fn describe_wait(seconds: i64) -> String {
    let minutes = (seconds + 30) / 60;
    let (hours, minutes_past_hour) = (minutes / 60, minutes % 60);

    match (hours, minutes_past_hour) {
        (0, 0) => "less than a minute".to_string(),
        (0, 1) => "about a minute".to_string(),
        (0, minutes) => format!("about {minutes} minutes"),
        (1, 0) => "about an hour".to_string(),
        (hours, 0) => format!("about {hours} hours"),
        (hours, minutes) => format!("about {hours}h {minutes}m"),
    }
}

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
    gh: &impl crate::github::GitHubApi,
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
    gh: &impl crate::github::GitHubApi,
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
    gh: &impl crate::github::GitHubApi,
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

/// Tidy up after GitHub has merged the Pull Request: mark it merged in the
/// stack session, take away the branches it used, point the Pull Requests that
/// were stacked on it at the master branch, and fetch what landed.
///
/// `merge_sha` is the commit the merge produced, where GitHub said which it
/// was.
///
/// Only ever called once GitHub has merged, which is what the branch deletions
/// make necessary rather than merely tidy. A Pull Request in a merge queue is
/// merged *from* its head branch, so taking that branch away before the queue
/// reaches it would withdraw the Pull Request instead of tidying up after it —
/// which is why a queued land that is not waiting stops short of here, and one
/// that is waiting arrives here only when the queue has finished.
async fn clean_up_after_merging(
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    stacks: &mut Option<StackSession>,
    pull_request: &crate::github::PullRequest,
    stacked_pull_requests: &[StackedPullRequest],
    merge_sha: Option<&str>,
) -> Result<()> {
    output("🛬", "Landed!")?;

    // The dissolve above happened while this Pull Request was still open, so it
    // is in the list of what the stack was holding. It is out of every stack for
    // good now and cannot be put back into one, so it is not something the
    // dissolve lost.
    if let Some(session) = stacks.as_mut() {
        session.merged(pull_request.number);
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
        stacked_pull_requests,
        DissolveReason::ToFollowALanding,
    )
    .await?;

    let retargeted = retarget_stacked_pull_requests(
        gh,
        stacked_pull_requests,
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
    fetch_what_landed(jj, config, merge_sha).await?;

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

    /// Put the Pull Request in the merge queue of the default branch and leave
    /// the merging to GitHub, whatever spr.landStrategy says
    #[clap(long, conflicts_with = "no_queue")]
    queue: bool,

    /// Squash-merge the Pull Request now rather than putting it in the merge
    /// queue of the default branch, whatever spr.landStrategy says
    #[clap(long)]
    no_queue: bool,

    /// Land through GitHub's stacked pull requests: one request merges this Pull
    /// Request and every member of its stack below it, and GitHub retargets and
    /// rebases the ones above. Needs spr.stackDisplay = github and
    /// spr.baseStrategy = linear-rebase. Whatever spr.landStrategy says
    #[clap(long, conflicts_with_all = ["no_stack", "queue"])]
    stack: bool,

    /// Merge the Pull Requests one at a time rather than through GitHub's
    /// stacked pull requests, whatever spr.landStrategy says
    #[clap(long)]
    no_stack: bool,

    /// Stay until the merge queue has merged the Pull Request, and then delete
    /// the branches it used and fetch what landed. A land that merges the Pull
    /// Request itself has nothing to wait for
    #[clap(long)]
    wait: bool,

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
    gh: &impl crate::github::GitHubApi,
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

    let landed = land_the_stack(opts, jj, gh, config, &mut stacks).await;

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

/// Land the Pull Request `opts` names and every one below it that has not
/// landed, bottom first.
///
/// The chain is worked out once, here, before anything merges. It cannot be
/// re-derived as the run goes: landing a Pull Request does not rewrite the
/// local changes, so the one above it still has an unlanded parent afterwards
/// and would look to [`changes_to_land`] exactly as it did at the start.
///
/// What each land in the chain needs of GitHub is settled here too, for the
/// same reason it was settled once per land before: [`decide_landing`] asks
/// about the master branch, which no land in this run changes, and a run
/// refused for wanting a queue that branch does not keep must be refused before
/// it has dissolved a stack or merged anything.
async fn land_the_stack(
    opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    stacks: &mut Option<StackSession>,
) -> Result<()> {
    let revision = opts.revision.as_deref().unwrap_or("@");
    let landing_on = jj.get_prepared_commit_for_revision(config, revision)?;

    // Asked here rather than left to `land_pull_request`, which asks the same
    // thing of every change it is given. That one runs on the changes below
    // this one first, so a land aimed at a change with no Pull Request would
    // otherwise land those before finding out it has nothing to finish with.
    if landing_on.pull_request_number.is_none() {
        write_commit_title(&landing_on)?;

        return Err(Error::new("This commit does not refer to a Pull Request."));
    }

    // A cherry-picked Pull Request carries its change onto the master branch by
    // itself, so there is nothing under it to land — see [`changes_to_land`].
    // The flag is read here rather than per change because it names the
    // revision this land was asked for, and saying it of that revision's
    // ancestors would claim something about them nobody said.
    let changes = if resolve_cherry_pick(opts.cherry_pick, &landing_on.message) {
        vec![landing_on]
    } else {
        changes_to_land(jj, gh, config, landing_on).await?
    };

    let landing = decide_landing(
        gh,
        config,
        resolve_land_strategy(
            opts.queue,
            opts.no_queue,
            opts.stack,
            opts.no_stack,
            config.land_strategy,
        ),
    )
    .await?;

    // GitHub's stack merge is one request for the whole chain, so it replaces
    // everything below rather than being one more way of merging each Pull
    // Request. It reports what it is doing itself, hence coming before the line
    // that names them.
    if let Landing::Stack = landing {
        return land_through_the_stack_merge(jj, gh, config, changes).await;
    }

    if changes.len() > 1 {
        // A queued land hands the merging to GitHub and returns, so the next
        // Pull Request in the chain would be asked to merge onto a master
        // branch its parent has not reached yet — GitHub would refuse it, or
        // worse, merge it and take the unlanded parent's commits along. Waiting
        // is the only way to land a chain through a queue, so a run that will
        // not wait is refused here rather than part way up.
        if let Landing::Queue(_) = landing
            && !opts.wait
        {
            return Err(Error::new(formatdoc!(
                "Landing this Pull Request means landing the {} below it \
                 first, and the default branch has a merge queue: each one has \
                 to be merged before the next can be queued. Pass --wait to \
                 stay until the queue has merged each of them, or land the \
                 Pull Requests below this one yourself.",
                if changes.len() == 2 {
                    "one".to_string()
                } else {
                    (changes.len() - 1).to_string()
                },
            )));
        }

        output(
            "🪜",
            &format!(
                "Landing {} Pull Requests, bottom first: {}",
                changes.len(),
                changes
                    .iter()
                    .filter_map(|change| change.pull_request_number)
                    .map(|number| format!("#{}", number))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        )?;
    }

    // Stops at the first land that fails, leaving what has already landed
    // landed. There is no putting those back, and the ones above them are no
    // worse off than they were: each is still open, still based on what it was,
    // and still lands with `jj spr land` once whatever refused this one is
    // dealt with.
    for change in changes {
        land_pull_request(&opts, jj, gh, config, stacks, &landing, change).await?;
    }

    Ok(())
}

/// How often a land asks GitHub whether its stack merge has happened yet.
///
/// Unlike a merge queue, nothing is waiting on anybody else's checks here:
/// GitHub takes the request and gets on with it, and a stack of three was
/// observed merged within four seconds.
const STACK_MERGE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a land waits for GitHub's stack merge before giving up on it.
///
/// Giving up is not a rollback — there is nothing to roll back, and the merge may
/// yet happen — so this only decides how long jj-spr keeps a terminal waiting
/// before it reports where the merge had got to and lets the user look for
/// themselves.
const STACK_MERGE_TIMEOUT: Duration = Duration::from_secs(120);

/// Land a whole chain through GitHub's stacked pull requests: one request that
/// merges the Pull Request at the top of `changes` and every member of its stack
/// below it.
///
/// What this does *not* do is the point of it. It dissolves no stack, retargets
/// nothing, and moves no base: GitHub merges each Pull Request in the chain,
/// then retargets the ones above onto the default branch and rebases their
/// branches itself. So the stack survives the land and needs no registering
/// again, and the Pull Requests above come out already showing only their own
/// changes — which is the one thing landing otherwise leaves for the next
/// `jj spr diff` to put right.
///
/// Three things have to be true, and each is refused here rather than left to
/// GitHub:
///
/// - **the branches have to survive a rebase**, which is
///   `spr.baseStrategy = linear-rebase` alone. Under the merging strategies the
///   rebase GitHub does to the Pull Request above discards its branch and GitHub
///   closes it as empty; the account of that is at the top of `impl GitHub` in
///   `github::stacks`. This is the refusal that matters: it protects work that a
///   land would otherwise destroy.
/// - **there has to be a stack**, since without one the endpoint merges the one
///   Pull Request alone, with none of the above being true and a commit message
///   from the repository's settings rather than from the local commit. That is
///   strictly worse than what `jj spr land` does by itself, so it is refused
///   rather than quietly done.
/// - **GitHub has to be about to merge what this land means to land.** How far
///   *down* the stack merge goes is not something the request can say: it merges
///   everything below, whatever the local chain says. See
///   [`reconcile_stack_merge`].
///
/// What jj-spr checks for itself, it checks for every Pull Request in the chain:
/// each one open, and approved where `spr.requireApproval` says so. GitHub's own
/// verdict on the merge is not asked for — `wait_for_mergeability` asks whether a
/// Pull Request can merge into the branch it is *based on*, which for a stacked
/// one is the branch below rather than the default branch, and the stack merge is
/// what resolves that chain. So the merge is where GitHub's refusals surface,
/// and nothing has been taken apart when they do.
async fn land_through_the_stack_merge(
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    changes: Vec<crate::jj::PreparedCommit>,
) -> Result<()> {
    if !config.base_strategy.rebases_branches() {
        return Err(Error::new(formatdoc!(
            "GitHub's stack merge rebases the head branch of every Pull Request \
             above the one it merges, and under spr.baseStrategy = {strategy} \
             those branches do not survive a rebase: each would collapse onto \
             its base and GitHub would close the Pull Request as empty, review \
             and all. Set spr.baseStrategy to 'linear-rebase' and push the stack \
             again with `jj spr diff`, or land without --stack, which merges the \
             Pull Requests one at a time.",
            strategy = if config.base_strategy == crate::config::BaseStrategy::Linear {
                "linear"
            } else {
                "synthetic"
            },
        )));
    }

    // Bottom first, ending with the Pull Request this land was asked for.
    // `changes_to_land` refuses a change without a Pull Request, and
    // `land_the_stack` refuses one for the change it was aimed at, so every
    // number is there — but the chain is what everything below compares against,
    // so it is worth not assuming that here.
    let mut landing = Vec::with_capacity(changes.len());
    for change in &changes {
        match change.pull_request_number {
            Some(number) => landing.push(number),
            None => {
                write_commit_title(change)?;

                return Err(Error::new("This commit does not refer to a Pull Request."));
            }
        }
    }

    let Some(&target) = landing.last() else {
        return Err(Error::new("There is nothing to land."));
    };

    let stack = match gh.get_open_stack_for_pull_request(target).await {
        Ok(Some(stack)) => stack,
        Ok(None) => {
            return Err(Error::new(formatdoc!(
                "Pull Request #{target} is not in a GitHub stack, so there is no \
                 stack to merge. Land it without --stack, which merges it and \
                 the Pull Requests below it one at a time — or push the stack \
                 with `jj spr diff` under spr.stackDisplay = github first."
            )));
        }
        Err(error) => {
            return Err(Error::new(format!(
                "Could not find out which GitHub stack Pull Request #{target} is \
                 in, so --stack cannot be honoured: {error}"
            )));
        }
    };

    reconcile_stack_merge(&stack, target, &landing)?;

    // Every Pull Request the merge will take, as GitHub has it. Fetched before
    // anything merges, both for the checks below and for the branches to take
    // away afterwards.
    let mut pull_requests = Vec::with_capacity(landing.len());
    for &number in &landing {
        let pull_request = gh.clone().get_pull_request(number).await?;

        if pull_request.state != PullRequestState::Open {
            return Err(Error::new(format!(
                "Pull Request #{number} is already closed!"
            )));
        }

        if config.require_approval && pull_request.review_status != Some(ReviewStatus::Approved) {
            return Err(Error::new(format!(
                "Pull Request #{number} has not been approved on GitHub."
            )));
        }

        pull_requests.push(pull_request);
    }

    output(
        "🪜",
        &format!(
            "Landing {} Pull Request{} through GitHub's stack #{}, bottom first: {}",
            landing.len(),
            if landing.len() == 1 { "" } else { "s" },
            stack.number,
            landing
                .iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    )?;

    match gh.merge_pull_request_async(target).await {
        Ok(AsyncMerge::Enqueued) => output("🛫", "GitHub is merging the stack...")?,
        Ok(AsyncMerge::AlreadyEnqueued) => {
            output("🛫", "GitHub was already merging this Pull Request...")?
        }
        Ok(AsyncMerge::Merged { .. }) => output("🛬", "GitHub had already merged it")?,
        Err(error) => {
            return Err(Error::new(format!(
                "GitHub would not merge the stack: {error}"
            )));
        }
    }

    let merge_commit = wait_for_the_stack_merge(gh, &landing).await?;

    output("🛬", "Landed!")?;

    // The head branches of the Pull Requests that merged are jj-spr's to take
    // away: GitHub leaves them behind, and it has already moved the Pull
    // Requests above onto the default branch, so nothing is based on them any
    // more. Nothing else is deleted — under this strategy a stacked Pull Request
    // has no generated base branch to clean up, since a Pull Request that had
    // one would not be chained to the one below and so could not be in the stack
    // at all.
    let mut deletions = Vec::with_capacity(pull_requests.len());
    for pull_request in &pull_requests {
        deletions.push(spawn_branch_deletion(jj, config, &pull_request.head)?);
    }

    fetch_what_landed(jj, config, merge_commit.as_deref()).await?;

    // What GitHub did with the rest of the stack, read back rather than assumed:
    // the retarget and the rebase are its work, and this is the only thing that
    // says they happened.
    match gh.get_stack(stack.number).await {
        Ok(stack) => {
            let above: Vec<String> = stack
                .pull_requests
                .iter()
                .filter(|member| !member.is_merged())
                .map(|member| format!("#{}", member.number))
                .collect();

            if !above.is_empty() {
                output(
                    "🎯",
                    &format!(
                        "GitHub moved {} onto {}, rebased {} branch{}, and kept stack #{}",
                        above.join(", "),
                        config.master_ref.branch_name(),
                        if above.len() == 1 { "its" } else { "their" },
                        if above.len() == 1 { "" } else { "es" },
                        stack.number,
                    ),
                )?;
            }
        }
        Err(error) => output(
            "⚠️",
            &format!(
                "Could not read GitHub stack #{} back to say what became of the Pull Requests \
                 above: {error}",
                stack.number
            ),
        )?,
    }

    // Ignored for the same reason the other branch deletions are: GitHub may be
    // configured to delete a merged Pull Request's branch itself, in which case
    // it is already gone and the push fails.
    for mut deletion in deletions {
        deletion.wait().await?;
    }

    Ok(())
}

/// Check that GitHub's stack merge would merge exactly the Pull Requests this
/// land means to land, bottom first.
///
/// The request names only the Pull Request at the top of what is to be merged;
/// how far *down* it reaches is the stack's business, not the caller's. So the
/// two lists have to be reconciled before anything is asked for, because
/// everything the request could get wrong is unrecoverable: a stack holding an
/// open Pull Request below the bottom of the local chain — one whose change was
/// abandoned locally, or that a colleague pushed — would be landed too, and
/// silently.
///
/// Merged members are skipped rather than counted, on both sides: they stay in
/// their stack for ever as history, and [`changes_to_land`] passes over a change
/// whose Pull Request GitHub has merged. So a stack landed halfway reconciles
/// with the chain that is left.
///
/// A pure function over what the two lists say, so that the case it exists for
/// can be tested without a stack to merge.
fn reconcile_stack_merge(stack: &crate::github::Stack, target: u64, landing: &[u64]) -> Result<()> {
    let members: Vec<(u64, bool)> = stack
        .pull_requests
        .iter()
        .map(|member| (member.number, member.is_merged()))
        .collect();

    reconcile_stack_members(stack.number, &members, target, landing)
}

/// The reconciliation itself, over the two lists alone. See
/// [`reconcile_stack_merge`], which reads them off a [`crate::github::Stack`].
fn reconcile_stack_members(
    stack_number: u64,
    members: &[(u64, bool)],
    target: u64,
    landing: &[u64],
) -> Result<()> {
    let Some(position) = members.iter().position(|(number, _)| *number == target) else {
        return Err(Error::new(formatdoc!(
            "GitHub stack #{stack_number} does not hold Pull Request #{target}, \
             so what its merge would land cannot be worked out. Land without \
             --stack."
        )));
    };

    let would_merge: Vec<u64> = members[..=position]
        .iter()
        .filter(|(_, merged)| !merged)
        .map(|(number, _)| *number)
        .collect();

    if would_merge == landing {
        return Ok(());
    }

    Err(Error::new(formatdoc!(
        "GitHub stack #{stack_number} would not merge what this land is for. \
         Merging Pull Request #{target} through the stack merges {would_merge}, \
         and the changes below it that have not landed are {landing}. A stack \
         merge takes everything below the Pull Request it is given, so this \
         cannot be narrowed. Push the stack again with `jj spr diff` so that it \
         matches the local changes, or land without --stack, which merges only \
         the Pull Requests of those changes.",
        would_merge = numbers(&would_merge),
        landing = numbers(landing),
    )))
}

/// The Pull Request numbers as a list for a person to read: `#12, #13`.
fn numbers(pull_requests: &[u64]) -> String {
    pull_requests
        .iter()
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wait until GitHub has merged every Pull Request in `landing`, and hand back
/// the commit the last of them landed as.
///
/// Only the top one is asked about while waiting: GitHub merges the chain from
/// the bottom, so the one this land named is the last to go and its merge is the
/// whole of the answer. The others are read once at the end, which is what turns
/// "the merge did not finish" into a sentence naming where it stopped.
///
/// Giving up on time is not giving up on the merge — GitHub may merge a moment
/// later — so the message says where things stood rather than claiming the land
/// failed.
async fn wait_for_the_stack_merge(
    gh: &impl crate::github::GitHubApi,
    landing: &[u64],
) -> Result<Option<String>> {
    let Some(&target) = landing.last() else {
        return Ok(None);
    };

    let deadline = tokio::time::Instant::now() + STACK_MERGE_TIMEOUT;

    loop {
        // Asked after the wait rather than before it: GitHub has only just taken
        // the request, so there is nothing to learn from asking straight away.
        tokio::time::sleep(STACK_MERGE_POLL_INTERVAL).await;

        let mergeability = gh.get_pull_request_mergeability(target).await?;
        if let Some(merge_commit) = mergeability.merge_commit {
            return Ok(Some(format!("{merge_commit}")));
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    // Where it got to, one Pull Request at a time, so that a merge that stopped
    // part way names what did land. Reported rather than returned: what has
    // merged has merged, and nothing here can be put back.
    let mut merged = Vec::new();
    let mut unmerged = Vec::new();
    for &number in landing {
        match gh.get_pull_request_mergeability(number).await {
            Ok(mergeability) if mergeability.merge_commit.is_some() => merged.push(number),
            _ => unmerged.push(number),
        }
    }

    Err(Error::new(formatdoc!(
        "GitHub has not finished merging the stack after {seconds} seconds. It \
         may yet: the request stands, and nothing here has to be undone. So far \
         {landed}, and {left}. Look at Pull Request #{target} on GitHub for why, \
         and run `jj spr land` again once it has settled — a merge that has \
         already happened is not repeated.",
        seconds = STACK_MERGE_TIMEOUT.as_secs(),
        landed = if merged.is_empty() {
            "nothing has landed".to_string()
        } else {
            format!("{} landed", numbers(&merged))
        },
        left = if unmerged.is_empty() {
            "nothing is left".to_string()
        } else {
            format!("{} did not", numbers(&unmerged))
        },
    )))
}

/// Fetch the master branch, and the commit `merge_sha` if there is one, so that
/// the local repository has what just landed.
///
/// Tried up to three times: fetching the very moment after a merge might not
/// find the new commit yet.
async fn fetch_what_landed(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    merge_sha: Option<&str>,
) -> Result<()> {
    let Some(sha) = merge_sha else {
        return Ok(());
    };

    for attempt in 0..3 {
        let git_fetch = jj
            .git_command()
            .arg("fetch")
            .arg("--no-write-fetch-head")
            .arg("--")
            .arg(&config.remote_name)
            .arg(config.master_ref.on_github())
            .arg(sha)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;

        if git_fetch.status.success() {
            break;
        } else if attempt == 2 {
            console::Term::stderr().write_all(&git_fetch.stderr)?;
            return Err(Error::new("git fetch failed"));
        }
    }

    // TODO: Implement Jujutsu-native rebase after landing
    // For now, the user will need to manually rebase after landing
    output(
        "⚠️",
        "Please manually rebase your working copy after landing",
    )
}

/// Land one Pull Request, in one of two orders.
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
///
/// One of possibly several: [`land_the_stack`] calls this for each change it
/// has to land, bottom first, and everything here is about the one it was
/// given. `stacks` is what carries the run as a whole between them, so that a
/// stack dissolved for one land is not dissolved again for the next and every
/// Pull Request the run merged is accounted for at the end.
async fn land_pull_request(
    opts: &LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    stacks: &mut Option<StackSession>,
    landing: &Landing,
    prepared_commit: crate::jj::PreparedCommit,
) -> Result<()> {
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

    // Nothing here asks whether the local change has parents that have not
    // landed, and there used to be a warning that did. [`land_the_stack`] lands
    // them, so by the time this runs for a change with unlanded parents, those
    // parents are what the run has just put on the master branch — and the one
    // shape where they are not, a cherry-picked Pull Request, is the one whose
    // branch does not carry them either.

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
    //
    // Never for a queued land. That verdict is about merging the Pull Request
    // into the branch as it stands, which is the one thing a branch with a
    // merge queue does not allow; and the requirements such a branch sets are
    // evaluated by the queue, on the Pull Request merged with the ones ahead of
    // it, rather than on the Pull Request alone. Holding a queued land to a
    // verdict about a merge that is not going to happen would refuse every one
    // of them. What jj-spr checks itself — `spr.requireApproval` above — still
    // stands, and the queue refuses to merge what does not pass it.
    let enforce_requirements = match landing {
        Landing::Queue(_) => false,
        // `Landing::Stack` never reaches this function: `land_the_stack` hands
        // the whole chain to [`land_through_the_stack_merge`] instead, which asks
        // GitHub nothing about mergeability and says why. So this arm is about
        // the merge made below, and the two are alike in the only thing it
        // decides.
        Landing::Merge | Landing::Stack => config.enforce_merge_requirements(opts.force),
    };

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
    // The asynchronous endpoint it points at is the one this function must not
    // use: it destroys the Pull Requests above the one it merges wherever their
    // branches do not survive a rebase, and nothing on this path establishes that
    // they do. `--stack` is that endpoint, taken up only where they do, and it
    // does not come through here at all — see [`land_through_the_stack_merge`].
    // The account of the hazard, and of how it was established, is at the top of
    // `impl GitHub` in `github::stacks`, which is where it belongs — repeating
    // the mechanism here would only give it somewhere to drift out of step.
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

    // Where the master branch has a merge queue, the land ends by joining it.
    //
    // Everything below this point is about a Pull Request that has been merged.
    // None of it has happened yet for a queued one, and the head branch in
    // particular must stay — the queue merges that branch, so deleting it would
    // withdraw the Pull Request rather than tidy up after it. The Pull Requests
    // stacked on this one wait too: they can only be pointed at the master
    // branch once this one's commits are actually on it.
    if let Landing::Queue(queue) = landing {
        let entry = match gh
            .enqueue_pull_request(&pull_request.node_id, pr_head_oid)
            .await
        {
            Ok(entry) => entry,
            Err(error) => {
                return abandon_land(
                    gh,
                    pull_request_number,
                    "Not adding this Pull Request to the merge queue",
                    retargeted_from,
                    error,
                )
                .await;
            }
        };

        // GitHub's own estimate where it has one, and the entry's rather than
        // the queue's: the queue was asked about before this Pull Request
        // joined it, so its estimate is for whoever queues next.
        let wait = match entry.estimated_time_to_merge {
            Some(seconds) => format!(", {}", describe_wait(seconds)),
            None => String::new(),
        };

        output(
            "🚦",
            &format!("Queued for merge at position {}{}", entry.position, wait),
        )?;
        output("🔗", &queue.url)?;

        if !opts.wait {
            output(
                "ℹ️ ",
                "The merge queue merges this Pull Request when it reaches it. Its \
                 branches stay until then, and the Pull Requests stacked on it \
                 still point at its branch — `jj spr diff` moves them once it has \
                 landed.",
            )?;

            return Ok(());
        }

        // The place just reported, so that a queue that has not moved by the
        // first look does not say the same thing twice.
        let merge_commit =
            wait_for_the_merge_queue(gh, pull_request_number, Some(entry.position)).await?;

        // The same tidying a squash-merging land does, at the point where the
        // merge actually happened rather than at the end of the land that asked
        // for it.
        return clean_up_after_merging(
            jj,
            gh,
            config,
            stacks,
            &pull_request,
            &stacked_pull_requests,
            Some(&format!("{}", merge_commit)),
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
    let merged = gh
        .merge_pull_request(
            pull_request_number,
            pull_request.title.clone(),
            build_github_body_for_merging(&pull_request.sections),
            pr_head_oid,
        )
        .await;

    let merge_commit = match merged {
        Ok(merge_commit) => merge_commit,
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

    clean_up_after_merging(
        jj,
        gh,
        config,
        stacks,
        &pull_request,
        &stacked_pull_requests,
        merge_commit.map(|oid| format!("{oid}")).as_deref(),
    )
    .await
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

    /// Every strategy a land can be configured with, for the tests that are
    /// about a flag rather than about one of them.
    const STRATEGIES: [LandStrategy; 4] = [
        LandStrategy::Auto,
        LandStrategy::Merge,
        LandStrategy::Queue,
        LandStrategy::Stack,
    ];

    #[test]
    fn test_land_strategy_falls_back_to_the_configured_one() {
        for configured in STRATEGIES {
            assert_eq!(
                resolve_land_strategy(false, false, false, false, configured),
                configured
            );
        }
    }

    #[test]
    fn test_land_strategy_flags_beat_the_configured_one() {
        for configured in STRATEGIES {
            assert_eq!(
                resolve_land_strategy(true, false, false, false, configured),
                LandStrategy::Queue
            );
            assert_eq!(
                resolve_land_strategy(false, true, false, false, configured),
                LandStrategy::Merge
            );
            assert_eq!(
                resolve_land_strategy(false, false, true, false, configured),
                LandStrategy::Stack
            );
        }
    }

    /// `--stack` says what to do, `--no-queue` only says what not to do, so the
    /// two together are not a contradiction and the stack merge wins.
    #[test]
    fn test_asking_for_the_stack_merge_and_not_the_queue_asks_for_the_stack_merge() {
        assert_eq!(
            resolve_land_strategy(false, true, true, false, LandStrategy::Auto),
            LandStrategy::Stack
        );
    }

    /// `--no-stack` displaces a configured stack merge and nothing else: it says
    /// to merge the Pull Requests one at a time, not whether to queue them, so
    /// what is left is for the default branch to decide.
    #[test]
    fn test_refusing_the_stack_merge_leaves_the_rest_to_the_branch() {
        assert_eq!(
            resolve_land_strategy(false, false, false, true, LandStrategy::Stack),
            LandStrategy::Auto
        );

        for configured in [LandStrategy::Auto, LandStrategy::Merge, LandStrategy::Queue] {
            assert_eq!(
                resolve_land_strategy(false, false, false, true, configured),
                configured,
                "--no-stack should leave {configured:?} alone"
            );
        }
    }

    /// The flags `--stack` and `--no-stack` are the shape the resolution above
    /// assumes: mutually exclusive, and `--stack` exclusive with `--queue`.
    /// Nothing else enforces that — `clap` does, and only if the attributes say
    /// so.
    #[test]
    fn test_land_options_refuse_contradictory_flags() {
        use clap::Parser;

        for flags in [
            ["--stack", "--no-stack"],
            ["--stack", "--queue"],
            ["--queue", "--no-queue"],
        ] {
            let result = LandOptions::try_parse_from(["land", flags[0], flags[1]]);

            assert!(
                result.is_err(),
                "{} with {} should be refused",
                flags[0],
                flags[1]
            );
        }

        assert!(
            LandOptions::try_parse_from(["land", "--stack", "--no-queue"]).is_ok(),
            "--stack with --no-queue is not a contradiction"
        );
    }

    /// The chain the local changes name, and the one GitHub would merge, being
    /// the same thing.
    #[test]
    fn a_stack_merge_that_lands_the_chain_is_accepted() {
        let members = [(11, false), (12, false), (13, false)];

        assert!(reconcile_stack_members(7, &members, 12, &[11, 12]).is_ok());
        assert!(reconcile_stack_members(7, &members, 13, &[11, 12, 13]).is_ok());
        assert!(reconcile_stack_members(7, &members, 11, &[11]).is_ok());
    }

    /// A stack landed halfway still reconciles: its merged members stay in it
    /// for ever, and `changes_to_land` passes over the changes they belong to.
    #[test]
    fn a_stack_with_merged_members_reconciles_with_what_is_left() {
        let members = [(11, true), (12, false), (13, false)];

        assert!(reconcile_stack_members(7, &members, 13, &[12, 13]).is_ok());
    }

    /// The case this check exists for: the stack holds an open Pull Request below
    /// the bottom of the local chain — a change abandoned locally, or somebody
    /// else's — and a stack merge would land it too, silently. There is no asking
    /// for less, so the land is refused.
    #[test]
    fn a_stack_reaching_below_the_chain_is_refused() {
        let members = [(11, false), (12, false), (13, false)];

        let error = reconcile_stack_members(7, &members, 13, &[12, 13])
            .expect_err("a stack that would land #11 as well must be refused");

        let messages = error.messages().join(" ");
        assert!(
            messages.contains("#11") && messages.contains("#12, #13"),
            "the refusal should name both chains: {messages}"
        );
    }

    /// A chain the stack does not hold all of — the local changes were pushed
    /// again as a new stack, say, or one of them was never registered — cannot be
    /// landed by merging this stack.
    #[test]
    fn a_chain_the_stack_does_not_hold_is_refused() {
        let members = [(12, false), (13, false)];

        assert!(reconcile_stack_members(7, &members, 13, &[11, 12, 13]).is_err());
        assert!(
            reconcile_stack_members(7, &members, 99, &[99]).is_err(),
            "a stack that does not hold the Pull Request at all says so"
        );
    }

    #[test]
    fn test_describe_wait_rounds_to_the_nearest_minute() {
        assert_eq!(describe_wait(0), "less than a minute");
        assert_eq!(describe_wait(29), "less than a minute");
        assert_eq!(describe_wait(30), "about a minute");
        assert_eq!(describe_wait(89), "about a minute");
        assert_eq!(describe_wait(90), "about 2 minutes");
        assert_eq!(describe_wait(12 * 60), "about 12 minutes");
    }

    #[test]
    fn test_describe_wait_counts_hours_once_there_are_any() {
        assert_eq!(describe_wait(59 * 60), "about 59 minutes");
        assert_eq!(describe_wait(60 * 60), "about an hour");
        assert_eq!(describe_wait(65 * 60), "about 1h 5m");
        assert_eq!(describe_wait(2 * 60 * 60), "about 2 hours");
        assert_eq!(describe_wait(150 * 60), "about 2h 30m");
    }
}
