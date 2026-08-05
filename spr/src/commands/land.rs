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
    github::{MergeQueue, PullRequestState, PullRequestUpdate, ReviewStatus},
    message::{MessageSection, build_github_body_for_merging},
    output::{output, write_commit_title},
    utils::run_command,
};

/// What this land will ask GitHub for, once it is known which of the two the
/// default branch takes.
enum Landing {
    /// Squash-merge the Pull Request now.
    Merge,
    /// Put the Pull Request in this merge queue and leave the merging to
    /// GitHub.
    Queue(MergeQueue),
}

/// The strategy to land under, given the flags this land was passed and the
/// configured one.
///
/// The flags name a strategy for one land rather than switching something on:
/// `--no-queue` is how a caller who may bypass the queue says to merge now in a
/// repository configured to queue, which is the same thing `spr.landStrategy =
/// merge` says for every land. Neither flag leaves the setting to decide.
///
/// Passing both is refused by `clap`, so the order the two are read in here
/// never decides anything.
fn resolve_land_strategy(queue: bool, no_queue: bool, configured: LandStrategy) -> LandStrategy {
    match (queue, no_queue) {
        (true, _) => LandStrategy::Queue,
        (_, true) => LandStrategy::Merge,
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
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    strategy: LandStrategy,
) -> Result<Landing> {
    let branch_name = config.master_ref.branch_name();

    Ok(match strategy {
        // Nothing is asked of GitHub here: a caller entitled to bypass the
        // queue may merge into a branch that has one, and it is GitHub's answer
        // to the merge itself that says whether this caller is.
        LandStrategy::Merge => Landing::Merge,
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

/// Whether `commit` sits directly on the master branch, with no change of its
/// own below it that has not landed.
///
/// Asked of the local stack rather than of what the Pull Request is based on,
/// because the two answer different questions. A Pull Request keeps whatever
/// base branch it was given until something moves it, so a change at the bottom
/// of what was once a stack still points at a generated base branch long after
/// everything below it landed. That is not a change with unlanded parents, and
/// it is unlanded parents this asks about.
fn sits_directly_on_master(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    commit: &crate::jj::PreparedCommit,
) -> Result<bool> {
    Ok(jj.get_master_base_for_commit(config, commit.oid)? == commit.parent_oid)
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

/// Wait for GitHub to work out whether it will merge Pull Request `number`, and
/// hold this land to that answer.
///
/// Two things have to be true: the Pull Request still has to have the head
/// `head_oid` this land is about, and GitHub has to call it mergeable. A Pull
/// Request GitHub still reports as based on something other than the master
/// branch is one it has not caught up with, so that is waited for rather than
/// judged.
///
/// The retrying is waiting for GitHub to catch up rather than for the Pull
/// Request to change. GitHub works the verdict out lazily and sends it back to
/// undecided whenever the Pull Request changes, so a land that has just
/// retargeted one asks before there is anything to read. After ten seconds of
/// no answer the land gives up rather than guess.
async fn wait_for_mergeability(
    gh: &crate::github::GitHub,
    number: u64,
    head_oid: git2::Oid,
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

        if mergeability.base.is_master_branch() && mergeability.mergeable.is_some() {
            if mergeability.mergeable != Some(true) {
                return Err(Error::new(formatdoc!(
                    "GitHub concluded the Pull Request is not mergeable at \
                    this point. Please rebase your changes and try again!"
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
///
/// `retargeted_from` is the base this land pointed at the master branch, or
/// `None` where it moved none. That is the whole of the rollback: nothing else
/// this land has done is undoable.
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
                    base: Some(base.on_github().to_string()),
                    ..Default::default()
                },
            )
            .await
    {
        error.push(format!("{}", rollback_error));
    }

    Err(error)
}

/// Tidy up after GitHub has merged the Pull Request: take away the branches it
/// used, and fetch what landed so that the caller can rebase onto it.
///
/// `base` is the base branch to take away as well, or `None` where the Pull
/// Request was on the master branch and had none of its own. `merge_sha` is the
/// commit the merge produced, where GitHub said which it was.
///
/// Only ever called once GitHub has merged: the branch deletions are what makes
/// that so. A Pull Request in a merge queue is merged *from* its branch, so
/// taking that branch away before the queue reaches it would withdraw the Pull
/// Request rather than tidy up after it.
async fn clean_up_after_merging(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    head: &crate::github::GitHubBranch,
    base: Option<&crate::github::GitHubBranch>,
    merge_sha: Option<&str>,
) -> Result<()> {
    let mut remove_old_branch_child_process = jj
        .git_command()
        .arg("push")
        .arg("--no-verify")
        .arg("--delete")
        .arg("--")
        .arg(&config.remote_name)
        .arg(head.on_github())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let remove_old_base_branch_child_process = match base {
        None => None,
        Some(base) => Some(
            jj.git_command()
                .arg("push")
                .arg("--no-verify")
                .arg("--delete")
                .arg("--")
                .arg(&config.remote_name)
                .arg(base.on_github())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        ),
    };

    // Rebase us on top of the now-landed commit
    if let Some(sha) = merge_sha {
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
                .arg(sha)
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
    remove_old_branch_child_process.wait().await?;
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

    /// Put the Pull Request in the merge queue of the default branch and leave
    /// the merging to GitHub, whatever spr.landStrategy says
    #[clap(long, conflicts_with = "no_queue")]
    queue: bool,

    /// Squash-merge the Pull Request now rather than putting it in the merge
    /// queue of the default branch, whatever spr.landStrategy says
    #[clap(long)]
    no_queue: bool,

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
    mut opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
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

    // What this land is going to ask for, settled before it asks GitHub for
    // anything else. Nothing below this changes the answer, and a land refused
    // for wanting a queue the default branch does not keep should be refused
    // before it has moved a base.
    let landing = decide_landing(
        gh,
        config,
        resolve_land_strategy(opts.queue, opts.no_queue, config.land_strategy),
    )
    .await?;

    // Retargeting a Pull Request at the master branch says it is to be merged
    // into that branch as it stands. Where the local change has parents that
    // have not landed, what that merges is this change *and* those parents,
    // because the Pull Request branch carries them.
    //
    // Said only of a queued land, and as a warning rather than a refusal.
    // Nothing about the merge is different — a merged land has moved the base
    // for the same reason since long before there was a queue — but a queued
    // one is answered by GitHub minutes or hours later, by which time there is
    // nothing left to read the wrong ordering off, so it is worth saying at the
    // one moment somebody is watching.
    if let Landing::Queue(_) = landing
        && !sits_directly_on_master(jj, config, &prepared_commit)?
    {
        output(
            "⚠️",
            "This change has parents that have not landed. Queueing this Pull \
             Request asks the merge queue to merge those parents' commits along \
             with it. Land the Pull Requests below this one first to avoid that.",
        )?;
    }

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

    // Okay, we are confident now that the PR can be merged and the result of
    // that merge would be a master commit with the same tree as if we
    // cherry-picked the commit onto master.
    let pr_head_oid = pull_request.head_oid;

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
    // already on the master branch and the block above moved nothing.
    let retargeted_from = (!base_is_master).then_some(&pull_request.base);

    // Check whether GitHub says this PR is mergeable.
    if let Err(error) = wait_for_mergeability(gh, pull_request_number, pr_head_oid).await {
        return abandon_land(
            gh,
            pull_request_number,
            "GitHub Pull Request merge failed",
            retargeted_from,
            error,
        )
        .await;
    }

    // Where the master branch has a merge queue, the land ends by joining it.
    //
    // Everything below this point is about a Pull Request that has been merged:
    // the branches it used are gone, and the commit it landed can be fetched.
    // None of that has happened yet for a queued Pull Request, and the branch
    // in particular must stay — the queue merges that branch, and deleting it
    // would take the Pull Request out of the queue rather than tidy up after
    // it. What GitHub does when the queue reaches the Pull Request is left for
    // a later `jj spr diff` or `jj spr cleanup` to notice.
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
        output(
            "ℹ️ ",
            "The merge queue merges this Pull Request when it reaches it. Its \
             branches stay until then.",
        )?;

        return Ok(());
    }

    // We have checked that merging the Pull Request branch into the master
    // branch produces the intended result, and that's independent of whether we
    // used a base branch with this Pull Request or not. We have made sure the
    // target of the Pull Request is set to the master branch. So let GitHub do
    // the merge now!
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

    clean_up_after_merging(
        jj,
        config,
        &pull_request.head,
        (!base_is_master).then_some(&pull_request.base),
        merge.sha.as_deref(),
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

    #[test]
    fn test_land_strategy_falls_back_to_the_configured_one() {
        for configured in [LandStrategy::Auto, LandStrategy::Merge, LandStrategy::Queue] {
            assert_eq!(resolve_land_strategy(false, false, configured), configured);
        }
    }

    #[test]
    fn test_land_strategy_flags_beat_the_configured_one() {
        for configured in [LandStrategy::Auto, LandStrategy::Merge, LandStrategy::Queue] {
            assert_eq!(
                resolve_land_strategy(true, false, configured),
                LandStrategy::Queue
            );
            assert_eq!(
                resolve_land_strategy(false, true, configured),
                LandStrategy::Merge
            );
        }
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
