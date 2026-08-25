/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::iter::zip;

use crate::{
    config::BaseStrategy,
    error::{Error, Result, ResultExt, add_error},
    github::{
        GitHub, GitHubBranch, PullRequest, PullRequestRequestReviewers, PullRequestState,
        PullRequestUpdate,
    },
    jj::{DryRunAction, StackChange},
    message::{MessageSection, build_stack_section, validate_commit_message},
    native_stacks::{
        ChainLink, DissolveReason, Reconciliation, StackSession, dissolve_any_stack_holding,
    },
    output::{output, write_commit_title},
    replay,
    utils::{parse_name_list, remove_all_parens, run_command},
};
use git2::Oid;
use indoc::{formatdoc, indoc};

#[derive(Debug, clap::Parser)]
pub struct DiffOptions {
    /// Create/update pull requests for commits in range from base to revision
    #[clap(long, short = 'a')]
    all: bool,

    /// Update the pull request title and description on GitHub from the local
    /// commit message
    #[clap(long)]
    update_message: bool,

    /// Submit any new Pull Request as a draft
    #[clap(long)]
    draft: bool,

    /// Message to be used for commits updating existing pull requests (e.g.
    /// 'rebase' or 'review comments')
    #[clap(long, short = 'm')]
    message: Option<String>,

    /// Submit this commit as if it was cherry-picked on master. Do not base it
    /// on any intermediate changes between the master branch and this commit.
    #[clap(long)]
    cherry_pick: bool,

    /// Remove the "Cherry Pick:" marker from the commit description and ignore
    /// it for this invocation. Future `jj spr diff` invocations will behave
    /// as though --cherry-pick was not specified.
    #[clap(long, conflicts_with = "cherry_pick")]
    no_cherry_pick: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Preview what would happen without pushing or creating PRs
    #[clap(long)]
    pub dry_run: bool,
}

/// Resolve the effective cherry-pick state from CLI flags and the "Cherry Pick:"
/// marker on the commit description.
///
/// When `--cherry-pick` is passed the marker is added (if absent); when
/// `--no-cherry-pick` is passed the marker is removed (if present). Returns
/// `(effective_cherry_pick, marker_was_changed)`.
fn resolve_cherry_pick(
    cherry_pick: bool,
    no_cherry_pick: bool,
    message: &mut crate::message::MessageSectionsMap,
) -> (bool, bool) {
    let stored = message
        .get(&MessageSection::CherryPick)
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if no_cherry_pick {
        if stored {
            message.remove(&MessageSection::CherryPick);
            return (false, true);
        }
        (false, false)
    } else if cherry_pick {
        if !stored {
            message.insert(MessageSection::CherryPick, "true".to_string());
            return (true, true);
        }
        (true, false)
    } else {
        (stored, false)
    }
}

pub async fn diff(
    opts: DiffOptions,
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

    // Get commits to process
    let mut prepared_commits = if use_range_mode {
        // Get range of commits from base to target
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        // Just get the single specified revision
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };

    let (Some(first_commit), Some(top_commit)) =
        (prepared_commits.first(), prepared_commits.last())
    else {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return result;
    };

    // Determine the master base OID - this is the commit on master that the stack is based on
    let master_base_oid = if use_range_mode {
        // For range mode, the parent of the first commit is the master base
        first_commit.parent_oid
    } else {
        // For single commit mode, find the actual merge base with master
        jj.get_master_base_for_commit(config, first_commit.oid)?
    };

    // Where the stack for the PR bodies starts. It runs back to the master branch
    // whatever revisions this run was asked to push, so that a PR is described
    // the same way however it was addressed. This is deliberately not
    // `master_base_oid`, which follows the range: a run bounded part-way up the
    // stack would then leave the PRs below the bound out of the section, even
    // though the pushed PRs are based on them and so genuinely are stacked on
    // them. Being on master, this commit is never rewritten, so an id for it
    // keeps.
    let stack_base_oid = jj.get_master_base_for_commit(config, top_commit.oid)?;

    // Identify the top of the stack now, while these commits still exist:
    // rewriting the messages below replaces them, so a commit id captured here
    // would be stale by the time the sections are worked out. A change id
    // survives that.
    let stack_top_change_id = jj.get_change_id_for_commit(top_commit.oid)?;

    // A change this run would open a pull request for has no number yet, and a
    // dry run opens nothing, so there is no number to give it. The stack it
    // ends up in is therefore one we cannot name, and a section worked out
    // without it would be wrong rather than merely incomplete — a change whose
    // neighbour is new would look like it was leaving the stack. So the
    // sections are only reported for a run that opens nothing.
    let would_create_pull_requests = prepared_commits
        .iter()
        .any(|commit| commit.pull_request_number.is_none());

    // Otherwise a dry run pushes nothing, so no pull request numbers appear
    // while it runs and the stack's shape is already settled. Working the
    // sections out before the loop rather than after it — the order a real run
    // has to use — lets each change weigh its own section before reporting
    // whether it is up to date. `--cherry-pick` writes no sections at all; see
    // below.
    let plan_sections = opts.dry_run && !opts.cherry_pick && !would_create_pull_requests;
    let stack_changes: HashMap<u64, StackChange> = if plan_sections {
        plan_stack_sections(jj, gh, config, stack_base_oid, &stack_top_change_id)
            .await?
            .into_iter()
            .map(|(number, _, change)| (number, change))
            .collect()
    } else {
        HashMap::new()
    };

    // Registering the run's pull requests as a stack GitHub draws itself is a
    // step around the loop rather than a way of running it: `None` is the whole
    // of "GitHub is not drawing the stack", so nothing below asks what mode this
    // is.
    let mut stacks = config
        .stack_display
        .draws_the_stack()
        .then(StackSession::new);

    // A stack GitHub draws is a stack GitHub offers to merge, and under one of
    // the base strategies that offer destroys the pull requests above the one
    // merged. Said once for the run, before anything is registered.
    if let Some(warning) = config.stack_shape_warning() {
        output("⚠️", warning)?;
    }

    // Every pull request is read before the loop pushes anything, rather than
    // as each change comes up. The lookups still run concurrently, but a run
    // that pushed the change below while the lookup for the change above it was
    // still in flight could read that pull request's base as the branch it had
    // just moved, and so measure the change against its own effects. Under
    // `spr.baseStrategy = linear-rebase` that is not a stale number but a wrong
    // one: the base is what says where the change's own commits start, and a
    // branch below that was rewritten shares no commit with the one the base
    // names.
    //
    // Concurrent on this task rather than spawned onto the runtime, which is
    // what lets the client be borrowed: a spawned future has to own one, and
    // owning one is the difference between a client this can be given and one
    // it has to be handed a clone of. The lookups are network waits, so there
    // is nothing for another thread to do with them anyway.
    let pull_requests = futures::future::join_all(prepared_commits.iter().map(|pc| async {
        match pc.pull_request_number {
            Some(number) => gh.get_pull_request(number).await.map(Some),
            None => Ok(None),
        }
    }))
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;

    let mut message_on_prompt = "".to_string();

    // The changes are walked bottom-up, so the change below the one being
    // pushed has already been pushed and can be offered to it as a base. See
    // [`linear_base`] for when that offer is taken up.
    let mut change_below: Option<PushedChange> = None;

    // The ordered pull requests of the run, which the registration needs and
    // which no revset can answer: whether one change's pull request is chained
    // to the one below is decided inside the loop, from trees and from what
    // GitHub already has. Accumulated bottom-up, as the loop walks.
    let mut links: Vec<ChainLink> = Vec::new();

    for (prepared_commit, pull_request) in zip(prepared_commits.iter_mut(), pull_requests) {
        if result.is_err() {
            break;
        }

        if !opts.dry_run {
            write_commit_title(prepared_commit)?;
        }

        // The further implementation of the diff command is in a separate function.
        // This makes it easier to run the code to update the local commit message
        // with all the changes that the implementation makes at the end, even if
        // the implementation encounters an error or exits early.
        let pushed = diff_impl(
            &opts,
            &mut message_on_prompt,
            jj,
            gh,
            config,
            prepared_commit,
            master_base_oid,
            pull_request,
            change_below.as_ref(),
            stacks.as_mut(),
            &stack_changes,
        )
        .await;

        match pushed {
            Ok(pushed) => {
                links.push(pushed.link());
                change_below = Some(pushed);
            }
            Err(error) => result = Err(error),
        }
    }

    // This updates the commit message in the local Jujutsu repository (if it was
    // changed by the implementation)
    if !opts.dry_run {
        add_error(
            &mut result,
            jj.rewrite_commit_messages(prepared_commits.as_mut_slice()),
        );
    }

    // A dry run opens no pull request, so a change that would get one has no
    // number, and a chain cannot be carried through a change it cannot name.
    // The chains such a run works out are therefore not the ones it would
    // register, and reporting them would be worse than reporting nothing: a
    // stack that would be appended to reads as unchanged, and a pull request
    // that would be registered again reads as one about to be left behind. So
    // the registration is only reported for a run that opens nothing.
    let plan_registration = !opts.dry_run || !would_create_pull_requests;

    // Now that every pull request the run pushed exists and is pointing where
    // it will end up, GitHub can be told that they are a stack.
    let mut registrations: Vec<Reconciliation> = Vec::new();
    if let Some(session) = stacks.as_mut()
        && result.is_ok()
        && plan_registration
        && let Some(outcomes) = add_error(
            &mut result,
            session.register(gh, &links, !opts.dry_run).await,
        )
    {
        registrations = outcomes;
    }

    // Asked however the run went, and after the registration either way: a run
    // that failed before it could register is exactly the one that may have
    // taken a stack apart and left it that way.
    if let Some(orphaned) = stacks
        .as_ref()
        .and_then(StackSession::orphaned_pull_requests)
    {
        registrations.push(orphaned);
    }

    if !opts.dry_run {
        for outcome in registrations.iter().filter(|o| o.is_notable()) {
            add_error(&mut result, report_stack(outcome));
        }
    }

    // Now that every commit this run touched has a PR, the stack's shape is
    // known and each PR can be told about it. This has to be a second pass:
    // during the loop above, the PRs for commits further up the stack may not
    // exist yet.
    //
    // Under `--cherry-pick` a PR keeps an existing base branch but is otherwise
    // based straight on master, so the run's PRs may or may not be stacked on
    // each other. There is no one stack shape to describe, so describe none.
    if !opts.dry_run && result.is_ok() && !opts.cherry_pick {
        add_error(
            &mut result,
            update_stack_sections(jj, gh, config, stack_base_oid, &stack_top_change_id).await,
        );
    }

    if opts.dry_run {
        // A change with only a stack section to rewrite still counts as work:
        // a real run would edit its pull request body.
        let actions: Vec<_> = prepared_commits
            .iter()
            .enumerate()
            .filter(|(_, c)| c.dry_run_action.is_some() || c.dry_run_stack_change.is_some())
            .collect();

        output(
            "\n📋",
            &format!(
                "Dry run complete. Would process {} change(s):\n",
                actions.len()
            ),
        )?;

        for (idx, pc) in &actions {
            let title = pc
                .message
                .get(&crate::message::MessageSection::Title)
                .map(|t| &t[..])
                .unwrap_or("");
            let pos = idx + 1;

            // `branches` is None for a change whose only work is its stack
            // section: nothing is pushed, so there is no head or base to name.
            let (action_label, branches, reviewers_list) = match &pc.dry_run_action {
                Some(DryRunAction::Create {
                    base,
                    head,
                    draft,
                    reviewers,
                    ..
                }) => {
                    let label = if *draft {
                        "CREATE (draft)".to_string()
                    } else {
                        "CREATE".to_string()
                    };
                    (
                        label,
                        Some((head.as_str(), base.as_str())),
                        reviewers.clone(),
                    )
                }
                Some(DryRunAction::Update {
                    pr_number,
                    base,
                    head,
                    ..
                }) => {
                    let label = format!("UPDATE PR #{pr_number}");
                    (label, Some((head.as_str(), base.as_str())), vec![])
                }
                None => {
                    let label = match pc.pull_request_number {
                        Some(number) => format!("UPDATE PR #{number} (body only)"),
                        None => "UPDATE (body only)".to_string(),
                    };
                    (label, None, vec![])
                }
            };

            output(
                &format!("  #{pos}"),
                &format!("{action_label}  {}  \"{title}\"", pc.short_id),
            )?;
            if let Some((head, base)) = branches {
                output("     ", &format!("head: {head}"))?;
                output("     ", &format!("base: {base}"))?;
            }
            if !reviewers_list.is_empty() {
                output(
                    "     ",
                    &format!("reviewers: {}", reviewers_list.join(", ")),
                )?;
            }
            if let Some(change) = pc.dry_run_stack_change {
                output("     ", &format!("stack: {}", change.describe()))?;
            }
            output("", "")?;
        }

        // What the registration would do cannot be worked out without asking
        // GitHub — the run's own pull requests say nothing about what stack
        // already holds them — so `register` above made that call even for a
        // dry run, and every chain reports, including the ones that turn out
        // not to be stacks.
        for outcome in &registrations {
            report_stack(outcome)?;
        }

        // An absent registration line must not read as "there is no GitHub
        // stack to register".
        if !plan_registration && config.stack_display.draws_the_stack() {
            output(
                "ℹ️",
                "The GitHub stack is not shown: this run would open pull \
                 requests, and a stack cannot be worked out until they have \
                 numbers.",
            )?;
        }

        // The same for the sections: their absence must not read as "the
        // sections are all up to date".
        if !plan_sections && !opts.cherry_pick && config.stack_display.writes_a_section() {
            output(
                "ℹ️",
                "Stack sections are not shown: this run would open pull \
                 requests, and the stack cannot be described until they have \
                 numbers.",
            )?;
        }
    }

    result
}

/// A change this run has already dealt with, offered to the change stacked on
/// top of it as the base its pull request could have.
///
/// Only [`diff_impl`] makes one, and only for a change that has a pull request
/// or is about to get one, so "the change below has no pull request" — one of
/// the conditions that sends a change back to a base branch of its own — needs
/// no field: it is the absence of a `PushedChange`.
#[derive(Clone, Debug)]
struct PushedChange {
    /// The local commit that was pushed. The change above this one is the one
    /// whose parent this is.
    local_oid: Oid,
    /// The commit `branch` now points at, which is what a change based on this
    /// one has to merge to contain it.
    head_oid: Oid,
    /// The pull request's head branch.
    branch: GitHubBranch,
    /// Whether the pushed commit carries this change cherry-picked onto
    /// master rather than the change's own tree, which makes it the wrong base
    /// for the change above: the diff would then leave out everything between
    /// master and this change.
    pushed_as_cherry_pick: bool,
    /// The change's pull request, once it has one.
    ///
    /// A pull request this run is opening has no number until GitHub answers
    /// with one, and a dry run never opens one, so this is `None` for a change
    /// whose pull request does not exist yet.
    pull_request_number: Option<u64>,
    /// The pull request this change's own is based on, where the base ref of
    /// this one is the head ref of that one. See [`chain_link`].
    based_on: Option<u64>,
    /// Whether the run moves this change's pull request onto a different base
    /// branch, which no stack holding it can survive.
    retargeted: bool,
}

impl PushedChange {
    /// How the change looks to the stack registration.
    fn link(&self) -> ChainLink {
        ChainLink {
            pull_request: self.pull_request_number,
            based_on: self.based_on,
            retargeted: self.retargeted,
        }
    }
}

/// Say what became of the run's stack registration.
fn report_stack(outcome: &Reconciliation) -> Result<()> {
    output("🧱", &format!("GitHub stack: {}", outcome.describe()))
}

/// The pull request below that this change's pull request is chained to, in the
/// sense GitHub's stacks mean: this pull request's base ref is that one's head
/// ref.
///
/// `linear_base` is not enough on its own to answer this. It says what the
/// change below *offers* to be, and the run may not take the offer up: a change
/// that needs no push exits before its base is looked at, so a stack migrating
/// to a linear `spr.baseStrategy` leaves pull requests whose trees are already
/// right still pointing at the synthetic base branches they had. Registering
/// those as a stack would be refused for not forming one. So the base branch
/// the pull request ends the run with is compared as well, which is the very
/// thing GitHub checks.
fn chain_link(
    linear_base: Option<&PushedChange>,
    base_branch: Option<&GitHubBranch>,
) -> Option<u64> {
    let below = linear_base?;

    if base_branch?.branch_name() != below.branch.branch_name() {
        return None;
    }

    below.pull_request_number
}

/// The pull request branch that the change above `change_below` should be
/// based on, if it should be based on one at all.
///
/// `None` means the change gets a synthetic base branch of its own (or master,
/// where it sits directly on master). That is always the answer under
/// [`BaseStrategy::Synthetic`], and is the fallback under either linear strategy
/// when the change below cannot serve as a base:
///
/// - there is no change below in this run, so nothing was pushed that this
///   change could point at;
/// - the change below is not this change's parent, because this run was given
///   revisions that do not form one chain;
/// - the change below was pushed as a cherry-pick, so its branch does not
///   carry the tree this change is built on;
/// - this change is itself a cherry-pick, and so belongs on master rather than
///   on anything the stack below it built;
/// - this change sits directly on master, which is what it is against then,
///   whatever was pushed below it.
///
/// Falling back is per change and never rewrites anything: the change simply
/// gets the base branch it would have had under the synthetic strategy.
///
/// A `Some` answer means the whole of case 0 applies, so that the decision is
/// made in one place: the caller uses it both to choose the base branch and to
/// decide that no base commit is to be built or pushed.
// The two flags are in the same order as in `determine_base_branch`, which is
// asked the same two questions a few lines away: swapping them would compile
// and would quietly change the base of every pull request in a stack.
fn linear_base(
    strategy: BaseStrategy,
    change_below: Option<&PushedChange>,
    parent_oid: Oid,
    directly_based_on_master: bool,
    cherry_pick: bool,
) -> Option<&PushedChange> {
    if !strategy.bases_on_the_change_below() || cherry_pick || directly_based_on_master {
        return None;
    }

    change_below.filter(|below| below.local_oid == parent_oid && !below.pushed_as_cherry_pick)
}

/// Determine which branch the pull request should be based on, and which one it
/// is based on now. Returns `(base_branch, old_base)`.
///
/// - `base_branch`: the branch the pull request should end up targeting, if it
///   already has one to keep or has one below to take over. `None` says only
///   that there is no such branch, not that the pull request belongs on master:
///   a change that is not on master and has no branch to point at gets one made
///   for it further down, where the commit for it is built. Nor is a `Some`
///   answer the last word — a change that needs a base commit built for it and
///   whose branch may not be written to gets one made for it too; see
///   [`may_push_base_commit_to`].
/// - `old_base`: the non-master branch the pull request targets now, if any.
///   That may well be the branch it ends up on, so compare the two before
///   treating it as one the pull request is leaving. Whether a branch it *is*
///   leaving may then be taken away is a separate question again, and one only
///   [`crate::config::Config::is_synthetic_base_branch`] answers: the branch a
///   pull request left under a linear strategy is the head branch of the
///   pull request below, and deleting it would close that one.
///
/// `cherry_pick` is whether this diff is a cherry-pick at all, which the
/// `Cherry Pick:` marker on the commit message answers as much as `--cherry-pick`
/// does — so pass the resolved answer, not the flag.
fn determine_base_branch(
    pull_request: Option<&PullRequest>,
    linear_base: Option<&PushedChange>,
    directly_based_on_master: bool,
    cherry_pick: bool,
) -> (Option<GitHubBranch>, Option<GitHubBranch>) {
    let old_base = pull_request
        .map(|pr| pr.base.clone())
        .filter(|base| !base.is_master_branch());

    let base_branch = if directly_based_on_master || cherry_pick {
        // The change is on master, so that is what the pull request is against.
        None
    } else if let Some(below) = linear_base {
        Some(below.branch.clone())
    } else {
        // Keep the base branch the pull request already has. That includes the
        // head branch of the change below, which is what a pull request pushed
        // under a linear strategy is based on: this run may not have the
        // change below in it — the default `jj spr diff` is one revision — but
        // that is no reason to move the pull request off a base that is still
        // right. What may not happen is *writing* to such a branch; see
        // [`may_push_base_commit_to`], which is asked at the one place a base
        // commit is built.
        old_base.clone()
    };

    (base_branch, old_base)
}

/// Whether a base commit this run builds may be pushed to `branch`.
///
/// A base branch jj-spr generated belongs to the one pull request based on it,
/// so it can be written to freely, and a branch jj-spr did not make at all is
/// treated as it always has been: the base commit goes there, as it did before
/// there was more than one base strategy. In between is the head branch of
/// another pull request, which is what [`BaseStrategy::Linear`] makes a stacked
/// pull request's base: pushing there would put a commit jj-spr invented into
/// somebody's pull request. Such a change gets a base branch of its own.
fn may_push_base_commit_to(config: &crate::config::Config, branch: &GitHubBranch) -> bool {
    !config.is_spr_branch(branch.branch_name())
        || config.is_synthetic_base_branch(branch.branch_name())
}

/// The parents of the new commit for a pull request branch, under the two
/// strategies that merge rather than replay.
///
/// The branch's previous tip always comes first, which is what makes every
/// update to such a pull request branch a fast-forward: the new commit descends
/// from the old one, so jj-spr never has to force-push and GitHub never loses
/// the review history. Whatever the base moved to is merged in as a second
/// parent — under [`BaseStrategy::Linear`] that is the head commit of the change
/// below, which is exactly what makes the base's branch an ancestor of this
/// one and so keeps GitHub's diff to this change alone.
///
/// [`BaseStrategy::LinearRebase`] wants the opposite of the merge this makes,
/// and does not come through here at all: see [`crate::replay`].
fn pr_head_parents(pr_head_oid: Oid, pr_base_parent: Option<Oid>) -> Vec<Oid> {
    let mut parents = vec![pr_head_oid];

    if let Some(oid) = pr_base_parent
        && oid != pr_head_oid
    {
        parents.push(oid);
    }

    parents
}

/// Push one change and create or update its pull request.
///
/// Returns what the change above it needs to know to be based on it: see
/// [`PushedChange`].
// Everything but `local_commit`, `pull_request` and `change_below` is the same
// for every change in the run, and is only threaded through because there is
// nothing yet to hold it. Those want bundling into a struct; until then the
// lint has nothing to tell us that the call site does not.
#[allow(clippy::too_many_arguments)]
async fn diff_impl(
    opts: &DiffOptions,
    message_on_prompt: &mut String,
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    local_commit: &mut crate::jj::PreparedCommit,
    master_base_oid: Oid,
    mut pull_request: Option<PullRequest>,
    change_below: Option<&PushedChange>,
    stacks: Option<&mut StackSession>,
    stack_changes: &HashMap<u64, StackChange>,
) -> Result<PushedChange> {
    // Parsed commit message of the local commit
    let message = &mut local_commit.message;

    let (effective_cherry_pick, marker_changed) =
        resolve_cherry_pick(opts.cherry_pick, opts.no_cherry_pick, message);
    if marker_changed {
        local_commit.message_changed = true;
    }

    // Check if the local commit is based directly on the master branch.
    let directly_based_on_master = local_commit.parent_oid == master_base_oid;

    // Whether the commit pushed for this change carries the change cherry-picked
    // onto master rather than the change's own tree. Cherry-picking a change
    // that is on master anyway gives that same tree, so it does not count.
    let pushed_as_cherry_pick = effective_cherry_pick && !directly_based_on_master;

    // The change below, if this change's pull request is to be based on it.
    let linear_base = linear_base(
        config.base_strategy,
        change_below,
        local_commit.parent_oid,
        directly_based_on_master,
        effective_cherry_pick,
    );

    // Determine the trees the Pull Request branch and the base branch should
    // have when we're done here.
    let (new_head_tree, new_base_tree) = if !effective_cherry_pick || directly_based_on_master {
        // Unless the user tells us to --cherry-pick, these should be the trees
        // of the current commit and its parent.
        // If the current commit is directly based on master (i.e.
        // directly_based_on_master is true), then we can do this here even when
        // the user tells us to --cherry-pick, because we would cherry pick the
        // current commit onto its parent, which gives us the same tree as the
        // current commit has, and the master base is the same as this commit's
        // parent.
        let head_tree = jj.get_tree_oid_for_commit(local_commit.oid)?;
        let base_tree = jj.get_tree_oid_for_commit(local_commit.parent_oid)?;

        (head_tree, base_tree)
    } else {
        // Cherry-pick the current commit onto master
        let index = jj.cherrypick(local_commit.oid, master_base_oid)?;

        if index.has_conflicts() {
            return Err(Error::new(formatdoc!(
                "This commit cannot be cherry-picked on {master}.",
                master = config.master_ref.branch_name(),
            )));
        }

        // This is the tree we are getting from cherrypicking the local commit
        // on master.
        let cherry_pick_tree = jj.write_index(index)?;
        let master_tree = jj.get_tree_oid_for_commit(master_base_oid)?;

        (cherry_pick_tree, master_tree)
    };

    if let Some(number) = local_commit.pull_request_number {
        output(
            "#️⃣ ",
            &format!(
                "Pull Request #{}: {}",
                number,
                config.pull_request_url(number)
            ),
        )?;
    }

    if local_commit.pull_request_number.is_none() || opts.update_message {
        validate_commit_message(message)?;
    }

    // A closed pull request is not necessarily one somebody closed. Deleting a
    // branch closes every pull request based on it
    // (`GitHubRule::DeletingABaseBranchClosesItsPullRequests`), so a landed
    // branch taken away before the pull requests above it were retargeted leaves
    // one closed in the middle of a stack, with nothing wrong with it but its
    // base. That is a repair, and `crate::reopen` is what makes it; anything
    // else is somebody's decision and is still refused.
    //
    // Done here, before any of the work below reads the pull request, for two
    // reasons that come to the same thing: everything downstream is entitled to
    // an open pull request whose branches exist, and the oids the pull request
    // carries are read from those branches — `get_pull_request` fetches them
    // both in one go, so a missing base leaves *both* stale or absent, and
    // `merge_base` on the result would fail below before the repair could
    // happen. So the repair is made and the pull request read again, after
    // which the run proceeds exactly as it would have had the branch never gone.
    //
    // `master_base_oid` is what the branch goes back at. Any commit reopens a
    // pull request, since a resurrected base ref is checked for existence and
    // not identity (`GitHubRule::AResurrectedBaseRefNeedsNoParticularCommit`),
    // and that one is on the master branch — so GitHub has it, this repository
    // has it, and the diff the pull request shows until it is retargeted is the
    // truthful "what this branch adds to master".
    let mut scaffold = None;

    // Decided before it is acted on, because acting on it replaces the pull
    // request this borrows.
    let missing_base = match pull_request.as_ref() {
        Some(closed) if closed.state == PullRequestState::Closed => {
            match crate::reopen::diagnose(gh, closed).await? {
                crate::reopen::Reopenable::ItsBaseBranchIsMissing => {
                    Some((closed.number, closed.base.clone()))
                }
                why => return Err(crate::reopen::cannot_be_put_back(closed.number, why)),
            }
        }
        _ => None,
    };

    if let Some((number, base)) = missing_base {
        if opts.dry_run {
            output(
                "🚑",
                &format!(
                    "Pull Request #{number} is closed because {} is gone from the \
                     remote. This run would put the branch back, reopen the Pull \
                     Request and retarget it",
                    base.branch_name()
                ),
            )?;

            // The branch is not on the remote, so neither is the commit read off
            // it — and everything below works from that commit. A real run puts
            // the branch back at `master_base_oid` before anything looks at it,
            // so a dry run plans against the same commit or it plans against a
            // zero oid and fails to plan at all.
            if let Some(closed) = pull_request.as_mut() {
                closed.base_oid = master_base_oid;
            }
        } else {
            scaffold = crate::reopen::put_back(gh, number, &base, master_base_oid).await?;

            output(
                "🚑",
                &format!(
                    "Reopened Pull Request #{number}, which was closed when {} was \
                     deleted out from under it",
                    base.branch_name()
                ),
            )?;

            // Read again rather than patched by hand: the reopen changed the
            // state, and the branch that has just come back is what the oids on
            // it are read from.
            pull_request = Some(gh.get_pull_request(number).await?);
        }
    }

    if let Some(ref pull_request) = pull_request
        && !opts.update_message
    {
        let mut pull_request_updates: PullRequestUpdate = Default::default();
        pull_request_updates.update_message(pull_request, message);

        if !pull_request_updates.is_empty() {
            output(
                "⚠️",
                indoc!(
                    "The Pull Request's title/message differ from the \
                     local commit's message.
                     Use `spr diff --update-message` to overwrite the \
                     title and message on GitHub with the local message, \
                     or `spr amend` to go the other way (rewrite the local \
                     commit message with what is on GitHub)."
                ),
            )?;
        }
    }

    // Parse "Reviewers" section, if this is a new Pull Request
    let mut requested_reviewers = PullRequestRequestReviewers::default();

    if local_commit.pull_request_number.is_none()
        && let Some(reviewers) = message.get(&MessageSection::Reviewers)
    {
        let reviewers = parse_name_list(reviewers);
        let mut checked_reviewers = Vec::new();

        for reviewer in reviewers {
            // Teams are indicated with a leading #
            if let Some(slug) = reviewer.strip_prefix('#') {
                if let Ok(team) = GitHub::get_github_team((&config.owner).into(), slug.into()).await
                {
                    requested_reviewers
                        .team_reviewers
                        .push(team.slug.to_string());

                    checked_reviewers.push(reviewer);
                } else {
                    return Err(Error::new(format!(
                        "Reviewers field contains unknown team '{}'",
                        reviewer
                    )));
                }
            } else if let Ok(user) = GitHub::get_github_user(reviewer.clone()).await {
                requested_reviewers.reviewers.push(user.login);
                if let Some(name) = user.name {
                    checked_reviewers.push(format!(
                        "{} ({})",
                        reviewer.clone(),
                        remove_all_parens(&name)
                    ));
                } else {
                    checked_reviewers.push(reviewer);
                }
            } else {
                return Err(Error::new(format!(
                    "Reviewers field contains unknown user '{}'",
                    reviewer
                )));
            }
        }

        message.insert(MessageSection::Reviewers, checked_reviewers.join(", "));
        local_commit.message_changed = true;
    }

    // Get the name of the existing Pull Request branch, or constuct one if
    // there is none yet.

    let title = message
        .get(&MessageSection::Title)
        .map(|t| &t[..])
        .unwrap_or("");

    let pull_request_branch = match &pull_request {
        Some(pr) => pr.head.clone(),
        None => {
            config.new_github_branch(&config.get_new_branch_name(&jj.get_all_ref_names()?, title))
        }
    };

    // Get the tree ids of the current head of the Pull Request, as well as the
    // base, and the commit id of the master commit this PR is currently based
    // on.
    // If there is no pre-existing Pull Request, we fill in the equivalent
    // values.
    let (pr_head_oid, pr_head_tree, pr_base_oid, pr_base_tree, pr_master_base) =
        if let Some(pr) = &pull_request {
            let pr_head_tree = jj.get_tree_oid_for_commit(pr.head_oid)?;

            let current_master_oid = jj.resolve_reference(config.master_ref.local())?;
            // Use git for merge base calculation since jj doesn't expose this directly
            let pr_base_oid = jj.git_repo.merge_base(pr.head_oid, pr.base_oid)?;
            let pr_base_tree = jj.get_tree_oid_for_commit(pr_base_oid)?;

            let pr_master_base = jj.git_repo.merge_base(pr.head_oid, current_master_oid)?;

            (
                pr.head_oid,
                pr_head_tree,
                pr_base_oid,
                pr_base_tree,
                pr_master_base,
            )
        } else {
            let master_base_tree = jj.get_tree_oid_for_commit(master_base_oid)?;
            (
                master_base_oid,
                master_base_tree,
                master_base_oid,
                master_base_tree,
                master_base_oid,
            )
        };
    let needs_merging_master = pr_master_base != master_base_oid;

    // Whether the branch is the shape and in the place that
    // [`BaseStrategy::LinearRebase`] wants, which the trees below say nothing
    // about. Two ways for a branch with perfectly good trees to still need
    // pushing:
    //
    // - the change below was rewritten in this run, so the commit this branch
    //   sits on is no longer the tip of the branch it is based on. GitHub would
    //   then diff this pull request from where the two branches last agreed,
    //   which is below the change below, and its changes would show up here.
    //   `pr.base_oid` was read before this run pushed anything, so the branch
    //   below having moved is exactly this comparison coming out unequal;
    // - the branch carries merge commits, because it was pushed under one of
    //   the other strategies. Left alone it would stay that way for as long as
    //   its trees stay right, and a rebase — GitHub's, when a stack is merged
    //   from its interface — would discard the change along with them.
    //
    // Only the early exit below asks this, and only for a pull request that
    // exists; for one being opened the branch does not exist yet and the answer
    // means nothing.
    let branch_is_as_this_strategy_wants = !config.base_strategy.rebases_branches()
        || (linear_base.is_none_or(|below| below.head_oid == pr_base_oid)
            && replay::is_a_chain(&jj.git_repo, pr_head_oid, pr_base_oid)?);

    // At this point we can check if we can exit early because no update to the
    // existing Pull Request is necessary
    if let Some(ref pull_request) = pull_request {
        // So there is an existing Pull Request...
        if !needs_merging_master
            && pr_head_tree == new_head_tree
            && pr_base_tree == new_base_tree
            && branch_is_as_this_strategy_wants
        {
            // ...and it does not need a rebase, and the trees of both Pull
            // Request branch and base are all the right ones.
            //
            // The stack section is written to the body in a pass of its own, so
            // "nothing to push" is not "nothing to do": the stack around this
            // change may have moved even though the change itself has not. Only
            // a dry run consults this, because only a dry run has to say up
            // front what the later pass will do.
            local_commit.dry_run_stack_change = stack_changes.get(&pull_request.number).copied();

            if let Some(change) = local_commit.dry_run_stack_change {
                output("📝", change.describe())?;
            } else {
                output("✅", "No update necessary")?;
            }

            if opts.update_message {
                // However, the user requested to update the commit message on
                // GitHub

                let mut pull_request_updates: PullRequestUpdate = Default::default();
                pull_request_updates.update_message(pull_request, message);

                if !pull_request_updates.is_empty() {
                    if opts.dry_run {
                        output(
                            "  ",
                            &format!("Would update PR #{} title/body", pull_request.number),
                        )?;
                    } else {
                        // ...and there are actual changes to the message
                        gh.update_pull_request(pull_request.number, pull_request_updates)
                            .await?;
                        output("✍", "Updated commit message on GitHub")?;
                    }
                }
            }

            // Nothing was pushed, so the branch still points where it did, and
            // that is what the change above this one has to build on. It is
            // still part of the stack the run is registering, though — a change
            // in the middle that needs no push must not sever the chain — so it
            // reports its pull request and what that is based on, which is the
            // base it already has rather than one this run chose.
            return Ok(PushedChange {
                local_oid: local_commit.oid,
                head_oid: pull_request.head_oid,
                branch: pull_request.head.clone(),
                pushed_as_cherry_pick,
                pull_request_number: Some(pull_request.number),
                based_on: chain_link(linear_base, Some(&pull_request.base)),
                // Nothing was pushed, so nothing was retargeted either.
                retargeted: false,
            });
        }
    }

    let (base_branch, old_base) = determine_base_branch(
        pull_request.as_ref(),
        linear_base,
        directly_based_on_master,
        effective_cherry_pick,
    );

    // We are going to construct `pr_base_parent: Option<Oid>`.
    // The value will be the commit we have to merge into the new Pull Request
    // commit to reflect changes in the parent of the local commit (by rebasing
    // or changing commits between master and this one, although technically
    // that's also rebasing).
    // If it's `None`, then we will not merge anything into the new Pull Request
    // commit.
    // If we are updating an existing PR, then there are four cases here:
    // (0) the change below this one has been pushed and its pull request branch
    //     already carries the tree this change is built on (the linear base
    //     strategy): that branch is the base, and no base commit is derived at
    //     all. It comes before case 1 because the base may be changing even
    //     when the trees say nothing has to be merged — a pull request moving
    //     off a base branch onto the branch below it has to gain that branch's
    //     tip as an ancestor in the same push, or GitHub would diff it against
    //     the wrong commit. `needs_merging_master` is not consulted: the change
    //     below is this change's parent and was pushed in this run, so whatever
    //     master it needed is already in its head commit, and merging that
    //     commit brings master in with it.
    // (1) the parent tree of this commit is unchanged and we do not need to
    //     merge in master, which means that the local commit was amended, but
    //     not rebased. We don't need to merge anything into the Pull Request
    //     branch.
    // (2) the parent tree has changed, but the parent of the local commit is on
    //     master (or we are cherry-picking) and we are not using a base branch:
    //     in this case we can merge the master commit we are based on into the
    //     PR branch, without going via a base branch. This also applies when
    //     the PR previously had a base branch — a synthetic one, or under the
    //     linear strategy the head branch of the change below — but the commit
    //     is now directly on master (e.g. after the bottom of a stack was
    //     landed). The PR is retargeted to master, and the branch it leaves is
    //     deleted only if jj-spr generated it as a base branch.
    // (3) the parent tree has changed, and we need to use a base branch (either
    //     because one was already created earlier, or the one the PR has is not
    //     ours to write to, or we find that we are not
    //     directly based on master now): we need to construct a new commit for
    //     the base branch. That new commit's tree is always that of that local
    //     commit's parent (thus making sure that the difference between base
    //     branch and pull request branch are exactly the changes made by the
    //     local commit, thus the changes we want to have reviewed). The new
    //     commit may have one or two parents. The previous base is always a
    //     parent (that's either the current commit on an existing base branch,
    //     or the previous master commit the PR was based on if there isn't a
    //     base branch already). In addition, if the master commit this commit
    //     is based on has changed, (i.e. the local commit got rebased on newer
    //     master in the meantime) then we have to merge in that master commit,
    //     which will be the second parent.
    // If we are creating a new pull request then `pr_base_tree` (the current
    // base of the PR) was set above to be the tree of the master commit the
    // local commit is based one, whereas `new_base_tree` is the tree of the
    // parent of the local commit. So if the local commit for this new PR is on
    // master, those two are the same (and we want to apply case 1). If a change
    // below is serving as its base, that is case 0, exactly as for an existing
    // pull request — case 0 is chosen before the pull request is consulted at
    // all. Otherwise, if the commit is not directly based on master, we have to
    // create this new PR with a base branch, so that is case 3.
    //
    // `base_branch_commit` is the commit to push to the base branch. It is the
    // same commit as `pr_base_parent` wherever this run built one, and `None`
    // in case 0, where the base branch is the branch below and the push for
    // that change is what moved it: merging its tip is this change's business,
    // writing to it is not.

    let (pr_base_parent, base_branch, base_branch_commit) = if let Some(below) = linear_base {
        // Case 0
        //
        // `graph_descendant_of` says no when the two commits are the same, so
        // that has to be asked separately: a branch tip already at the change
        // below needs no merge.
        let contains_below = pr_head_oid == below.head_oid
            || jj
                .git_repo
                .graph_descendant_of(pr_head_oid, below.head_oid)?;

        (
            if contains_below {
                None
            } else {
                Some(below.head_oid)
            },
            // `determine_base_branch` has already answered with this branch,
            // being asked the same question. Saying so here rather than
            // trusting that keeps "case 0 targets the branch below" a fact
            // about these lines: were the two ever to disagree, the pull
            // request would be retargeted at master while its head merged the
            // branch below, and its diff would swallow the whole stack.
            base_branch.or_else(|| Some(below.branch.clone())),
            None,
        )
    } else if pr_base_tree == new_base_tree && !needs_merging_master {
        // Case 1
        (None, base_branch, None)
    } else if base_branch.is_none() && (directly_based_on_master || effective_cherry_pick) {
        // Case 2
        (Some(master_base_oid), None, None)
    } else {
        // Case 3

        // We are constructing a base branch commit.
        // One parent of the new base branch commit will be the current base
        // commit, that could be either the top commit of an existing base
        // branch, or a commit on master.
        let mut parents = vec![pr_base_oid];

        // If we need to rebase on master, make the master commit also a
        // parent (except if the first parent is that same commit, we don't
        // want duplicates in `parents`).
        if needs_merging_master && pr_base_oid != master_base_oid {
            parents.push(master_base_oid);
        }

        let new_base_branch_commit = if opts.dry_run {
            // Use a placeholder OID — this won't be pushed
            pr_base_oid
        } else {
            jj.create_derived_commit(
                local_commit.parent_oid,
                &format!(
                    "[spr] {}\n\n[skip ci]",
                    if pull_request.is_some() {
                        "changes introduced through rebase".to_string()
                    } else {
                        format!(
                            "changes to {} this commit is based on",
                            config.master_ref.branch_name()
                        )
                    },
                ),
                new_base_tree,
                &parents[..],
            )?
        };

        // The commit has to go somewhere: onto the base branch the pull request
        // already has, where that is a branch we may write to, and otherwise
        // onto a `GitHubBranch` with a new name for a base branch. The pull
        // request is retargeted at whichever it turns out to be.
        let base_branch = match base_branch {
            Some(base_branch) if may_push_base_commit_to(config, &base_branch) => base_branch,
            _ => config
                .new_github_branch(&config.get_base_branch_name(&jj.get_all_ref_names()?, title)),
        };

        (
            Some(new_base_branch_commit),
            Some(base_branch),
            Some(new_base_branch_commit),
        )
    };

    // Worked out here because the branches below take `base_branch` apart, and
    // because a change that ends the run based on the one below it is part of
    // the stack whether or not this run was what put it there.
    let based_on = chain_link(linear_base, base_branch.as_ref());

    // Whether this run moves the pull request onto a different base branch.
    //
    // This is the one predicate for it, and the three places below that send a
    // base to GitHub are exactly the cases it covers: retargeting off a base
    // branch that has become obsolete, setting a base that has changed, and
    // going back to the master branch. Each of those requires the base to
    // differ from the one the pull request has, and no other path sends one.
    // Deriving them all from one expression is what keeps the unstacking below
    // from drifting out of step with the retargeting it has to precede.
    let retargeted = pull_request.as_ref().is_some_and(|pull_request| {
        let target = base_branch.as_ref().unwrap_or(&config.master_ref);

        pull_request.base.branch_name() != target.branch_name()
    });

    // The pull request the change ends the run with. One this run is opening
    // has no number until GitHub answers with one, below.
    let mut pull_request_number = local_commit.pull_request_number;

    // Under [`BaseStrategy::LinearRebase`] the branch is not written past but
    // rebuilt: the change's own commits are replayed onto whatever the base
    // moved to, so that the branch stays a chain of single-parent commits. That
    // is what `pr_base_parent` means to the other strategies too — the commit
    // the base moved to — and `pr_base_oid` is where the change's own commits
    // sit now, so a run that finds them already on it replays nothing.
    //
    // Done before the message for the new commit is asked for, because a rebuild
    // that already ends at the change's tree adds no commit and so needs no
    // message: a pure rebase under this strategy is the replay and nothing else.
    // A dry run rebuilds nothing at all, since building the commits is the work
    // it exists to not do.
    let rebuilt = if config.base_strategy.rebases_branches() && !opts.dry_run {
        let rebuilt = replay::rebuild(
            &jj.git_repo,
            pr_head_oid,
            pr_base_oid,
            pr_base_parent.unwrap_or(pr_base_oid),
        )?;

        if let Some((emoji, message)) = rebuilt.describe(pull_request_branch.branch_name()) {
            output(emoji, &message)?;
        }

        Some(rebuilt)
    } else {
        None
    };

    // Whether the change's tree still has to be committed on top of what the
    // branch carries. Only a rebuild can answer no — the merging strategies
    // always make a commit, even for a pure rebase, since merging the new base
    // in *is* that commit.
    let commits_the_tree = rebuilt
        .as_ref()
        .is_none_or(|rebuilt| rebuilt.tip_tree != new_head_tree);

    let mut github_commit_message = opts.message.clone();
    if pull_request.is_some()
        && github_commit_message.is_none()
        && !opts.dry_run
        && commits_the_tree
    {
        let input = {
            let message_on_prompt = message_on_prompt.clone();

            tokio::task::spawn_blocking(move || {
                dialoguer::Input::<String>::new()
                    .with_prompt("Message (leave empty to abort)")
                    .with_initial_text(message_on_prompt)
                    .allow_empty(true)
                    .interact_text()
            })
            .await??
        };

        if input.is_empty() {
            return Err(Error::new("Aborted as per user request".to_string()));
        }

        *message_on_prompt = input.clone();
        github_commit_message = Some(input);
    }

    // The commit the Pull Request branch will point at. Where the branch was
    // rebuilt, the change's tree goes on top of what the replay ended at — and
    // where the replay already ended at that tree, nothing is added and the
    // branch is the replay. Otherwise the new commit is written past the branch's
    // current tip, merging in whatever the base moved to; see
    // [`pr_head_parents`].
    let pr_commit = if opts.dry_run {
        // Use a placeholder OID — this won't be pushed
        pr_head_oid
    } else {
        let message = github_commit_message
            .as_ref()
            .map(|s| &s[..])
            .unwrap_or_else(|| title);

        match &rebuilt {
            Some(rebuilt) if !commits_the_tree => rebuilt.tip,
            Some(rebuilt) => {
                jj.create_derived_commit(local_commit.oid, message, new_head_tree, &[rebuilt.tip])?
            }
            None => jj.create_derived_commit(
                local_commit.oid,
                message,
                new_head_tree,
                &pr_head_parents(pr_head_oid, pr_base_parent)[..],
            )?,
        }
    };

    if opts.dry_run {
        let base_ref = base_branch.as_ref().unwrap_or(&config.master_ref);
        let base_branch_name = base_ref.branch_name();
        let head_branch_name = pull_request_branch.branch_name();
        let is_stacked = !base_ref.is_master_branch();

        local_commit.dry_run_action = if let Some(ref pr) = pull_request {
            // A change can need both a push and a rewritten stack section.
            local_commit.dry_run_stack_change = stack_changes.get(&pr.number).copied();

            Some(crate::jj::DryRunAction::Update {
                pr_number: pr.number,
                base: base_branch_name.to_string(),
                head: head_branch_name.to_string(),
                is_stacked,
            })
        } else {
            let all_reviewers: Vec<String> = requested_reviewers
                .reviewers
                .iter()
                .chain(requested_reviewers.team_reviewers.iter())
                .cloned()
                .collect();
            Some(crate::jj::DryRunAction::Create {
                base: base_branch_name.to_string(),
                head: head_branch_name.to_string(),
                is_stacked,
                draft: opts.draft,
                reviewers: all_reviewers,
            })
        };
    } else {
        let mut cmd = jj.git_command();
        cmd.arg("push").arg("--atomic").arg("--no-verify");

        // A branch whose commits were replayed no longer descends from what the
        // remote has, so this push cannot be a fast-forward. The lease names the
        // commit GitHub reported when this run read the pull request, so a push
        // that would overwrite anything else — a colleague's commit, a branch
        // GitHub rebased itself when a stack was merged — is refused rather than
        // quietly winning. Only the head branch is leased: a base commit goes to
        // a branch that is written past, never rewritten.
        if rebuilt.as_ref().is_some_and(|rebuilt| rebuilt.rewritten) {
            cmd.arg(format!(
                "--force-with-lease={}:{}",
                pull_request_branch.on_github(),
                pr_head_oid
            ));
        }

        cmd.arg("--").arg(&config.remote_name).arg(format!(
            "{}:{}",
            pr_commit,
            pull_request_branch.on_github()
        ));

        // Where this run prepared a new commit for a base branch, that goes in
        // the same push. Case 0 builds no such commit — the base is the branch
        // below, which its own push moved — so the branch here is either one
        // generated for this pull request or one jj-spr did not make at all,
        // which it writes to as it did before there were base strategies. See
        // [`may_push_base_commit_to`], which draws that line.
        if let (Some(base_branch), Some(base_branch_commit)) = (&base_branch, base_branch_commit) {
            // Where a repair put this very branch back so the pull request could
            // be reopened, the commit it went back at was chosen to reopen the
            // pull request and nothing else, and it has no reason to be an
            // ancestor of the one being pushed now. Leased to the scaffold, so
            // the force applies to the commit this run put there and to no
            // other: a branch someone else has written to since is a rejected
            // push, exactly as it would have been without the repair.
            if let Some(scaffold) = scaffold
                .as_ref()
                .filter(|scaffold| scaffold.branch().branch_name() == base_branch.branch_name())
            {
                cmd.arg(format!(
                    "--force-with-lease={}:{}",
                    base_branch.on_github(),
                    scaffold.at()
                ));
            }

            cmd.arg(format!(
                "{}:{}",
                base_branch_commit,
                base_branch.on_github()
            ));
        }

        if let Some(pull_request) = pull_request {
            // We are updating an existing Pull Request

            if needs_merging_master {
                output(
                    "⚾",
                    &format!(
                        "Commit was rebased - updating Pull Request #{}",
                        pull_request.number
                    ),
                )?;
            } else {
                output(
                    "🔁",
                    &format!(
                        "Commit was changed - updating Pull Request #{}",
                        pull_request.number
                    ),
                )?;
            }

            // Things we want to update in the Pull Request on GitHub
            let mut pull_request_updates: PullRequestUpdate = Default::default();

            if opts.update_message {
                pull_request_updates.update_message(&pull_request, message);
            }

            // Push the new commit onto the Pull Request branch (and also the new
            // base commit, if we added that to cmd above). It comes before both
            // of the branches below because either may ask GitHub to diff the
            // pull request against a different base, and the head branch — with
            // any base branch this run built for it — has to be on the remote in
            // the state that base assumes before that is asked for.
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;

            // GitHub refuses to change the base of a pull request that is in a
            // stack, so any stack holding this one has to go first. Every path
            // below that sends a base does so only when `retargeted`, so asking
            // here rather than at each of them is what stops the condition
            // drifting from the calls it has to precede; see where `retargeted`
            // is worked out for why one predicate covers all of them.
            if retargeted {
                dissolve_any_stack_holding(
                    stacks,
                    gh,
                    pull_request.number,
                    DissolveReason::ToMoveABase,
                )
                .await?;
            }

            // The branch the pull request ends this run based on, named before
            // the branches below take `base_branch` apart, because what becomes
            // of a repair's scaffold is settled after them and by this.
            let final_base = base_branch
                .as_ref()
                .unwrap_or(&config.master_ref)
                .branch_name()
                .to_string();

            if let Some(base_branch) = base_branch {
                // We are using a base branch.

                // A base branch of ours that the Pull Request is moving off —
                // which is what a stack migrating to `spr.baseStrategy =
                // linear` does — is nobody's once it has moved, so it goes.
                // Retargeting and deleting have to happen in that order and the
                // retarget has to be confirmed first, which is why this does not
                // go through `pull_request_updates` with everything else.
                let obsolete_base = old_base.as_ref().filter(|old| {
                    old.branch_name() != base_branch.branch_name()
                        && config.is_synthetic_base_branch(old.branch_name())
                });

                if let Some(obsolete_base) = obsolete_base {
                    let deleted = gh
                        .retarget_pull_request(pull_request.number, &base_branch, obsolete_base)
                        .await?;

                    output(
                        "🎯",
                        &format!(
                            "Retargeted Pull Request #{} to {}",
                            pull_request.number,
                            base_branch.branch_name()
                        ),
                    )?;

                    if deleted {
                        output("🗑️", &format!("Deleted {}", obsolete_base.branch_name()))?;
                    }
                } else if pull_request.base.branch_name() != base_branch.branch_name() {
                    // If the Pull Request's base is not set to the base branch
                    // yet, change that now.
                    pull_request_updates.base = Some(base_branch.branch_name().to_string());
                }
            } else {
                // The Pull Request is against the master branch (or we are
                // retargeting it to master), so there was no base branch of
                // ours in the push above. There may still be one it is moving
                // off: retarget it to master and, where that branch was ours,
                // take it away — in that order, because a base branch deleted
                // while the pull request still points at it closes the pull
                // request.
                if let Some(ref old_base) = old_base {
                    let deleted = gh
                        .retarget_to_master_branch(pull_request.number, old_base)
                        .await?;

                    output(
                        "🎯",
                        &format!(
                            "Retargeted Pull Request #{} to {}",
                            pull_request.number,
                            config.master_ref.branch_name()
                        ),
                    )?;

                    if deleted {
                        output("🗑️", &format!("Deleted {}", old_base.branch_name()))?;
                    }
                }
            }

            if !pull_request_updates.is_empty() {
                gh.update_pull_request(pull_request.number, pull_request_updates)
                    .await?;
            }

            // The branch a repair put back is the repair's to take away, and now
            // is when it may go: the retargeting above is done and GitHub has
            // confirmed it, so nothing points at the branch any more and taking
            // it away closes nothing. A branch this run went on to make the
            // pull request's real base is not a scaffold any more and stays.
            //
            // A run that fails between the repair and here leaves the branch
            // behind, and deliberately so — it is the base of an open pull
            // request by then, and taking it away would close the pull request
            // this run has just put back. It is left for the next run to
            // retarget off and delete, or for `jj spr cleanup` to sweep once
            // nothing is based on it.
            if let Some(scaffold) = scaffold.take() {
                if scaffold.branch().branch_name() == final_base {
                    // Kept, and deliberately not reported: the branch is where
                    // the pull request lives now, so there is nothing left of
                    // the repair to say.
                } else {
                    let name = scaffold.branch().branch_name().to_string();

                    if scaffold.take_away(gh).await? {
                        output("🗑️", &format!("Deleted {name}"))?;
                    }
                }
            }
        } else {
            // We are creating a new Pull Request.

            // Push the pull request branch and the base branch if present.
            //
            // Deliberately a second copy of the push rather than one hoisted
            // above `if let Some(pull_request)`: hoisting it would put the push
            // ahead of the "updating Pull Request #N" line the other branch
            // prints first. Nothing else holds it here — a pull request that
            // does not exist yet is in no stack, so the unstacking the other
            // branch does after its push has nothing to do on this side.
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;

            // Then call GitHub to create the Pull Request.
            let created = gh
                .create_pull_request(
                    message,
                    base_branch
                        .as_ref()
                        .unwrap_or(&config.master_ref)
                        .branch_name()
                        .to_string(),
                    pull_request_branch.branch_name().to_string(),
                    opts.draft,
                )
                .await?;

            pull_request_number = Some(created);
            let pull_request_url = config.pull_request_url(created);

            output(
                "✨",
                &format!(
                    "Created new Pull Request #{}: {}",
                    created, &pull_request_url
                ),
            )?;

            message.insert(MessageSection::PullRequest, pull_request_url);
            local_commit.message_changed = true;

            let result = gh.request_reviewers(created, requested_reviewers).await;
            match result {
                Ok(()) => (),
                Err(error) => {
                    output("⚠️", "Requesting reviewers failed")?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            }
        }
    }

    // `pr_commit` is where the pull request branch now points — or would, on a
    // dry run, where it still points, which is enough for the change above to
    // report the branch it would be based on.
    Ok(PushedChange {
        local_oid: local_commit.oid,
        head_oid: pr_commit,
        branch: pull_request_branch,
        pushed_as_cherry_pick,
        pull_request_number,
        based_on,
        retargeted,
    })
}

/// Tell every PR in the stack above `stack_base_oid` what the stack looks like,
/// or take the section away where it is not what the repository wants.
///
/// The stack comes from the repository rather than from the revisions this run
/// was asked to push, and is deliberately wider than them: it runs from where
/// the stack leaves master up through the top change's descendants, so that
/// pushing one change still brings its neighbours' sections up to date, and so
/// that a PR is described the same way however it was addressed. Commits
/// without a PR — an empty working copy change, or one never diffed — are
/// simply not part of the list.
///
/// Under any [`StackDisplay`](crate::config::StackDisplay) but `section` this
/// writes `None`, which *removes* a section rather than skipping it. That is
/// what makes the setting switchable: a repository moving to `github`, or to
/// `none`, would otherwise leave every PR carrying a list that nothing updates
/// any more, and two descriptions of a stack that disagree are worse than one.
/// The removal costs a lookup per PR and no write at all where there is no
/// section to take away, since the update is then empty.
async fn update_stack_sections(
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    stack_base_oid: Oid,
    stack_top_change_id: &str,
) -> Result<()> {
    for (number, update, _) in
        plan_stack_sections(jj, gh, config, stack_base_oid, stack_top_change_id).await?
    {
        gh.update_pull_request(number, update).await?;
    }

    Ok(())
}

/// Work out which pull requests in the stack need their section rewritten.
///
/// Split out of [`update_stack_sections`] so that a dry run can report the
/// same work without doing it. Only pull requests that would actually change
/// are returned, so an empty result means every section is already right.
async fn plan_stack_sections(
    jj: &crate::jj::Jujutsu,
    gh: &impl crate::github::GitHubApi,
    config: &crate::config::Config,
    stack_base_oid: Oid,
    stack_top_change_id: &str,
) -> Result<Vec<(u64, crate::github::PullRequestUpdate, StackChange)>> {
    let commits = jj.get_stack_commits(config, stack_base_oid, stack_top_change_id)?;
    let numbers: Vec<u64> = commits
        .iter()
        .filter_map(|commit| commit.pull_request_number)
        .collect();

    let mut planned = Vec::new();
    for number in &numbers {
        let section = config
            .stack_display
            .writes_a_section()
            .then(|| build_stack_section(&numbers, *number))
            .flatten();
        let pull_request = gh.get_pull_request(*number).await?;

        let mut update = crate::github::PullRequestUpdate::default();
        update.update_stack_section(&pull_request, section.as_deref());

        if !update.is_empty() {
            let had = pull_request.sections.get(&MessageSection::Stack);
            // The body is what decides whether there is anything to send, so a
            // section that reads the same either side is a re-render of one
            // that is already there, which is a rewrite like any other.
            let change = match (had, &section) {
                (None, Some(_)) => StackChange::Added,
                (Some(_), None) => StackChange::Removed,
                _ => StackChange::Rewritten,
            };
            planned.push((*number, update, change));
        }
    }

    Ok(planned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config() -> crate::config::Config {
        crate::config::Config::new(
            "test_owner".into(),
            "test_repo".into(),
            "origin".into(),
            "main".into(),
            "spr/test/".into(),
            false,
        )
    }

    #[allow(dead_code)]
    fn create_test_git_repo() -> (TempDir, git2::Repository) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let repo = git2::Repository::init(temp_dir.path()).expect("Failed to init git repo");

        // Create initial commit
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");
        let tree_id = {
            let mut index = repo.index().expect("Failed to get index");
            index.write_tree().expect("Failed to write tree")
        };
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Initial commit",
            &tree,
            &[],
        )
        .expect("Failed to create initial commit");

        drop(tree); // Drop the tree reference before moving repo
        (temp_dir, repo)
    }

    #[allow(dead_code)]
    fn create_test_commit(repo: &git2::Repository, message: &str, content: &str) -> git2::Oid {
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");

        // Write content to a test file
        let repo_path = repo.workdir().expect("Failed to get workdir");
        let file_path = repo_path.join("test.txt");
        fs::write(&file_path, content).expect("Failed to write test file");

        // Add file to index
        let mut index = repo.index().expect("Failed to get index");
        index
            .add_path(std::path::Path::new("test.txt"))
            .expect("Failed to add file to index");
        index.write().expect("Failed to write index");

        let tree_id = index.write_tree().expect("Failed to write tree");
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        // Get HEAD commit as parent
        let parent_commit = repo
            .head()
            .expect("Failed to get HEAD")
            .peel_to_commit()
            .expect("Failed to peel to commit");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )
        .expect("Failed to create commit")
    }

    #[test]
    fn test_diff_options_default_values() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: None,
            revision: None,
            dry_run: false,
        };

        assert!(!opts.all);
        assert!(!opts.update_message);
        assert!(!opts.draft);
        assert!(!opts.cherry_pick);
        assert!(opts.message.is_none());
        assert!(opts.base.is_none());
    }

    #[test]
    fn test_diff_options_with_base() {
        let opts = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            dry_run: false,
        };

        assert_eq!(opts.base, Some("main".to_string()));
        assert!(opts.all);
    }

    #[test]
    fn test_jujutsu_integration() {
        // Test configuration for jj-spr
        let config = create_test_config();
        assert_eq!(config.owner, "test_owner");
        assert_eq!(config.remote_name, "origin");
    }

    #[test]
    fn test_base_option_parsing() {
        // Test that the base option can be parsed correctly
        let opts_with_base = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            dry_run: false,
        };

        assert_eq!(opts_with_base.base.as_deref(), Some("main"));
        assert!(opts_with_base.all);

        let opts_with_trunk = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
        };

        assert_eq!(opts_with_trunk.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_all_flag_behavior() {
        let opts_with_all = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
        };

        // When --all is specified, it should work with base revisions
        assert!(opts_with_all.all);
        assert!(opts_with_all.base.is_some());
    }

    #[test]
    fn test_diff_options_combinations() {
        // Test various valid combinations of options
        let opts = DiffOptions {
            all: true,
            update_message: true,
            draft: true,
            message: Some("Update message".to_string()),
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
        };

        assert!(opts.all);
        assert!(opts.update_message);
        assert!(opts.draft);
        assert_eq!(opts.message.as_deref(), Some("Update message"));
        assert!(!opts.cherry_pick);
        assert_eq!(opts.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_diff_options_dry_run_flag() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: None,
            revision: None,
            dry_run: true,
        };

        assert!(opts.dry_run);
        assert!(!opts.all);
    }

    // -------------------------------------------------------------------------
    // resolve_cherry_pick unit tests

    fn make_map_with_cherry_pick(value: &str) -> crate::message::MessageSectionsMap {
        [(
            crate::message::MessageSection::CherryPick,
            value.to_string(),
        )]
        .into()
    }

    #[test]
    fn test_resolve_cherry_pick_default_no_marker() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(false, false, &mut map);
        assert!(!effective);
        assert!(!changed);
        assert!(!map.contains_key(&crate::message::MessageSection::CherryPick));
    }

    #[test]
    fn test_resolve_cherry_pick_default_with_marker() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(false, false, &mut map);
        assert!(effective);
        assert!(!changed);
    }

    #[test]
    fn test_resolve_cherry_pick_marker_case_insensitive() {
        let mut map = make_map_with_cherry_pick("TRUE");
        let (effective, _) = resolve_cherry_pick(false, false, &mut map);
        assert!(effective);
    }

    #[test]
    fn test_resolve_cherry_pick_marker_other_value() {
        for value in &["false", "yes", "1", ""] {
            let mut map = make_map_with_cherry_pick(value);
            let (effective, _) = resolve_cherry_pick(false, false, &mut map);
            assert!(!effective, "Expected false for marker value {:?}", value);
        }
    }

    #[test]
    fn test_resolve_cherry_pick_flag_inserts_marker() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(true, false, &mut map);
        assert!(effective);
        assert!(changed);
        assert_eq!(
            map.get(&crate::message::MessageSection::CherryPick)
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn test_resolve_cherry_pick_flag_idempotent() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(true, false, &mut map);
        assert!(effective);
        assert!(!changed);
    }

    #[test]
    fn test_resolve_cherry_pick_no_flag_removes_marker() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(false, true, &mut map);
        assert!(!effective);
        assert!(changed);
        assert!(!map.contains_key(&crate::message::MessageSection::CherryPick));
    }

    #[test]
    fn test_resolve_cherry_pick_no_flag_when_absent() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(false, true, &mut map);
        assert!(!effective);
        assert!(!changed);
    }

    #[test]
    fn test_diff_options_parse_no_cherry_pick() {
        use clap::Parser;
        let opts = DiffOptions::try_parse_from(["test", "--no-cherry-pick"]).unwrap();
        assert!(opts.no_cherry_pick);
        assert!(!opts.cherry_pick);
    }

    #[test]
    fn test_diff_options_cherry_pick_no_cherry_pick_conflict() {
        use clap::Parser;
        let result = DiffOptions::try_parse_from(["test", "--cherry-pick", "--no-cherry-pick"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    fn make_test_pr(base_branch_name: &str) -> PullRequest {
        PullRequest {
            number: 42,
            node_id: "PR_test".to_string(),
            state: PullRequestState::Open,
            title: "test".to_string(),
            body: None,
            sections: Default::default(),
            base: crate::github::GitHubBranch::new_from_branch_name(
                base_branch_name,
                "origin",
                "main",
            ),
            head: crate::github::GitHubBranch::new_from_branch_name(
                "spr/test/my-feature",
                "origin",
                "main",
            ),
            base_oid: git2::Oid::zero(),
            head_oid: git2::Oid::zero(),
            merge_commit: None,
            reviewers: Default::default(),
            review_status: None,
        }
    }

    /// A change below that a linear base can be taken from: it is the parent of
    /// the change under test, and was pushed as itself rather than
    /// cherry-picked.
    fn make_change_below(branch_name: &str) -> PushedChange {
        PushedChange {
            local_oid: parent_oid(),
            head_oid: git2::Oid::from_str("2222222222222222222222222222222222222222").unwrap(),
            branch: crate::github::GitHubBranch::new_from_branch_name(
                branch_name,
                "origin",
                "main",
            ),
            pushed_as_cherry_pick: false,
            pull_request_number: Some(41),
            based_on: None,
            retargeted: false,
        }
    }

    /// The commit the change under test is stacked on.
    fn parent_oid() -> Oid {
        git2::Oid::from_str("1111111111111111111111111111111111111111").unwrap()
    }

    #[test]
    fn test_determine_base_branch_no_pr_returns_none() {
        let (base, old) = determine_base_branch(None, None, true, false);
        assert!(base.is_none());
        assert!(old.is_none());
    }

    #[test]
    fn test_determine_base_branch_pr_on_master_returns_none() {
        let pr = make_test_pr("main");
        let (base, old) = determine_base_branch(Some(&pr), None, true, false);
        assert!(base.is_none());
        assert!(old.is_none());
    }

    #[test]
    fn test_determine_base_branch_stacked_pr_not_on_master_keeps_the_base_it_has() {
        let pr = make_test_pr("spr/test/main.parent-feature");
        let (base, old) = determine_base_branch(Some(&pr), None, false, false);
        assert_eq!(base.unwrap().branch_name(), "spr/test/main.parent-feature");
        assert_eq!(old.unwrap().branch_name(), "spr/test/main.parent-feature");
    }

    #[test]
    fn test_determine_base_branch_drops_synthetic_when_directly_on_master() {
        let pr = make_test_pr("spr/test/main.parent-feature");
        let (base, old) = determine_base_branch(Some(&pr), None, true, false);
        assert!(
            base.is_none(),
            "should drop synthetic base when directly on master"
        );
        assert_eq!(
            old.unwrap().branch_name(),
            "spr/test/main.parent-feature",
            "should report the base it is leaving"
        );
    }

    #[test]
    fn test_determine_base_branch_drops_synthetic_when_cherry_pick() {
        let pr = make_test_pr("spr/test/main.parent-feature");
        let (base, old) = determine_base_branch(Some(&pr), None, false, true);
        assert!(base.is_none(), "should drop synthetic base on cherry-pick");
        assert!(old.is_some(), "should report the base it is leaving");
    }

    #[test]
    fn test_determine_base_branch_pr_on_master_not_directly_on_master() {
        let pr = make_test_pr("main");
        let (base, old) = determine_base_branch(Some(&pr), None, false, false);
        assert!(base.is_none(), "PR already on master stays on master");
        assert!(old.is_none(), "no synthetic base to clean up");
    }
    /// The whole point of the linear strategy: the pull request is based on the
    /// branch of the change below rather than on a branch of its own.
    #[test]
    fn a_linear_base_is_the_branch_of_the_change_below() {
        let below = make_change_below("spr/test/parent-feature");
        let (base, _) =
            determine_base_branch(Some(&make_test_pr("main")), Some(&below), false, false);

        assert_eq!(base.unwrap().branch_name(), "spr/test/parent-feature");
    }

    /// A change that has no pull request yet is based on the change below just
    /// the same, so that the first push of a stack is already linear.
    #[test]
    fn a_new_pull_request_takes_the_linear_base_too() {
        let below = make_change_below("spr/test/parent-feature");
        let (base, old) = determine_base_branch(None, Some(&below), false, false);

        assert_eq!(base.unwrap().branch_name(), "spr/test/parent-feature");
        assert!(old.is_none());
    }

    /// Moving a pull request off its synthetic base branch has to report that
    /// branch as the one it is leaving, or the branch would be left on the
    /// remote with nothing pointing at it.
    #[test]
    fn a_linear_base_replaces_a_synthetic_one() {
        let below = make_change_below("spr/test/parent-feature");
        let (base, old) = determine_base_branch(
            Some(&make_test_pr("spr/test/main.parent-feature")),
            Some(&below),
            false,
            false,
        );

        assert_eq!(base.unwrap().branch_name(), "spr/test/parent-feature");
        assert_eq!(old.unwrap().branch_name(), "spr/test/main.parent-feature");
    }

    /// A run that does not contain the change below — which the default
    /// `jj spr diff` never does, since it is given one revision — must leave a
    /// pull request based on the branch below where it is. It is still the
    /// right base; only writing to it is out of the question.
    #[test]
    fn a_run_without_the_change_below_keeps_the_base_it_has() {
        let (base, old) = determine_base_branch(
            Some(&make_test_pr("spr/test/parent-feature")),
            None,
            false,
            false,
        );

        assert_eq!(base.unwrap().branch_name(), "spr/test/parent-feature");
        assert_eq!(old.unwrap().branch_name(), "spr/test/parent-feature");
    }

    /// A branch jj-spr did not make is left alone: the pull request keeps it,
    /// as it did before there was more than one base strategy.
    #[test]
    fn a_base_branch_that_is_not_ours_is_kept() {
        let (base, old) = determine_base_branch(
            Some(&make_test_pr("someones-release-branch")),
            None,
            false,
            false,
        );

        assert_eq!(base.unwrap().branch_name(), "someones-release-branch");
        assert_eq!(old.unwrap().branch_name(), "someones-release-branch");
    }

    /// The head branch of the pull request below is what a stacked pull request
    /// is based on under the linear strategy. A base commit pushed there would
    /// turn up in that pull request, so the change gets a base branch of its
    /// own instead — this is what keeps turning the strategy back off from
    /// writing into somebody else's review.
    #[test]
    fn no_base_commit_is_pushed_to_a_pull_request_head_branch() {
        let config = create_test_config();
        let head_branch = config.new_github_branch("spr/test/parent-feature");

        assert!(!may_push_base_commit_to(&config, &head_branch));
    }

    /// The branch jj-spr generates for a stacked pull request to be based on
    /// is what the base commit is built for in the first place.
    #[test]
    fn a_base_commit_is_pushed_to_a_base_branch_of_ours() {
        let config = create_test_config();
        let base_branch = config.new_github_branch("spr/test/main.parent-feature");

        assert!(may_push_base_commit_to(&config, &base_branch));
    }

    /// A branch outside jj-spr's namespace is somebody's deliberate choice of
    /// base, and is treated as it always has been.
    #[test]
    fn a_base_commit_is_pushed_to_a_branch_that_is_not_ours() {
        let config = create_test_config();
        let branch = config.new_github_branch("someones-release-branch");

        assert!(may_push_base_commit_to(&config, &branch));
    }

    /// The default strategy builds each pull request a base branch of its own,
    /// so what was pushed below is nothing to it.
    #[test]
    fn the_synthetic_strategy_ignores_the_change_below() {
        let below = make_change_below("spr/test/parent-feature");

        assert!(
            linear_base(
                BaseStrategy::Synthetic,
                Some(&below),
                parent_oid(),
                false, // directly_based_on_master
                false, // cherry_pick
            )
            .is_none()
        );
    }

    /// The whole of the linear strategies in one assertion: the change below is
    /// what the pull request above is based on. Both of them, because they
    /// differ in how the head branch is built and not in what it is based on —
    /// a `linear-rebase` run that fell back to a base branch of its own would
    /// build the very merge commits it exists to avoid.
    #[test]
    fn the_linear_strategies_take_the_change_below() {
        for strategy in [BaseStrategy::Linear, BaseStrategy::LinearRebase] {
            let below = make_change_below("spr/test/parent-feature");
            let taken = linear_base(
                strategy,
                Some(&below),
                parent_oid(),
                false, // directly_based_on_master
                false, // cherry_pick
            )
            .unwrap_or_else(|| panic!("{strategy:?} should base on the change below"));

            assert_eq!(taken.branch.branch_name(), "spr/test/parent-feature");
        }
    }

    /// The bottom of a run has nothing below it, and so nothing to be based on.
    #[test]
    fn there_is_no_linear_base_without_a_change_below() {
        assert!(
            linear_base(
                BaseStrategy::Linear,
                None,
                parent_oid(),
                false, // directly_based_on_master
                false, // cherry_pick
            )
            .is_none()
        );
    }

    /// A run given revisions that do not form one chain would otherwise base a
    /// change on a branch that does not carry the tree it is built on.
    #[test]
    fn a_change_below_that_is_not_the_parent_is_not_a_base() {
        let mut below = make_change_below("spr/test/parent-feature");
        below.local_oid = git2::Oid::from_str("3333333333333333333333333333333333333333").unwrap();

        assert!(
            linear_base(
                BaseStrategy::Linear,
                Some(&below),
                parent_oid(),
                false, // directly_based_on_master
                false, // cherry_pick
            )
            .is_none()
        );
    }

    /// A cherry-picked change below was pushed as it would look on master, so
    /// its branch is the wrong tree to diff the change above against.
    #[test]
    fn a_cherry_picked_change_below_is_not_a_base() {
        let mut below = make_change_below("spr/test/parent-feature");
        below.pushed_as_cherry_pick = true;

        assert!(
            linear_base(
                BaseStrategy::Linear,
                Some(&below),
                parent_oid(),
                false, // directly_based_on_master
                false, // cherry_pick
            )
            .is_none()
        );
    }

    /// A change being cherry-picked belongs on master, whatever it is stacked
    /// on locally.
    #[test]
    fn a_cherry_picked_change_takes_no_linear_base() {
        let below = make_change_below("spr/test/parent-feature");

        assert!(
            linear_base(
                BaseStrategy::Linear,
                Some(&below),
                parent_oid(),
                false, // directly_based_on_master
                true,  // cherry_pick
            )
            .is_none()
        );
    }

    /// A change on master is against master, whatever was pushed below it.
    #[test]
    fn a_change_on_master_takes_no_linear_base() {
        let below = make_change_below("spr/test/parent-feature");

        assert!(
            linear_base(
                BaseStrategy::Linear,
                Some(&below),
                parent_oid(),
                true,  // directly_based_on_master
                false, // cherry_pick
            )
            .is_none()
        );
    }

    /// The link GitHub's stacks are made of: this pull request's base ref is
    /// the head ref of the pull request below.
    #[test]
    fn a_pull_request_based_on_the_one_below_is_chained_to_it() {
        let below = make_change_below("spr/test/parent-feature");
        let base = below.branch.clone();

        assert_eq!(chain_link(Some(&below), Some(&base)), Some(41));
    }

    /// The case `linear_base` alone gets wrong. A change whose trees are
    /// already right needs no push, so the run never moves it off the synthetic
    /// base branch it has — while the change below still offers itself as a
    /// base. Registering that as a stack would be refused for not forming one.
    #[test]
    fn a_pull_request_left_on_its_synthetic_base_is_not_chained() {
        let below = make_change_below("spr/test/parent-feature");
        let base = crate::github::GitHubBranch::new_from_branch_name(
            "spr/test/main.parent-feature",
            "origin",
            "main",
        );

        assert_eq!(chain_link(Some(&below), Some(&base)), None);
    }

    /// A pull request against the master branch has no base branch at all, and
    /// so is the bottom of whatever stack it is in.
    #[test]
    fn a_pull_request_on_master_is_chained_to_nothing() {
        let below = make_change_below("spr/test/parent-feature");

        assert_eq!(chain_link(Some(&below), None), None);
    }

    /// Nothing below means nothing to be chained to, whatever the base says.
    #[test]
    fn a_pull_request_with_nothing_below_is_chained_to_nothing() {
        let base = crate::github::GitHubBranch::new_from_branch_name(
            "spr/test/parent-feature",
            "origin",
            "main",
        );

        assert_eq!(chain_link(None, Some(&base)), None);
    }

    /// A change below whose pull request this run is opening has no number
    /// until GitHub answers with one, and a dry run never asks. There is
    /// nothing to name it by, so the chain stops.
    #[test]
    fn a_pull_request_that_does_not_exist_yet_chains_nothing_to_it() {
        let mut below = make_change_below("spr/test/parent-feature");
        below.pull_request_number = None;
        let base = below.branch.clone();

        assert_eq!(chain_link(Some(&below), Some(&base)), None);
    }

    /// Under the merging strategies a pull request branch only ever moves
    /// forward: every commit put on it descends from what was there before,
    /// whatever else it merges in, so the push needs no `--force` and GitHub
    /// keeps the review comments on every commit. `linear-rebase` gives this up
    /// deliberately and does not build its commits here at all — see
    /// [`crate::replay`] — so this stays a statement about the other two.
    #[test]
    fn a_merged_pull_request_branch_only_ever_moves_forward() {
        let head = git2::Oid::from_str("4444444444444444444444444444444444444444").unwrap();
        let base = git2::Oid::from_str("5555555555555555555555555555555555555555").unwrap();

        for merged in [None, Some(base), Some(head)] {
            let parents = pr_head_parents(head, merged);
            assert_eq!(
                parents.first(),
                Some(&head),
                "the branch tip must stay the first parent, merging {merged:?}"
            );
        }

        // ...and a merge of the commit that is already there is not a merge.
        assert_eq!(pr_head_parents(head, Some(head)), vec![head]);
        assert_eq!(pr_head_parents(head, Some(base)), vec![head, base]);
        assert_eq!(pr_head_parents(head, None), vec![head]);
    }
}
