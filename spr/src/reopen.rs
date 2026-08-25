/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Putting back a pull request GitHub closed because a branch went missing.
//!
//! Deleting a branch closes the pull requests that name it
//! (`GitHubRule::DeletingABaseBranchClosesItsPullRequests`), and it closes them
//! from *both* sides: the one whose head branch it was, and the one based on it.
//! The second of those is a pull request nobody meant to close, and in a stack
//! it is a pull request in the *middle* — GitHub's merge queue deleting a landed
//! branch before it has retargeted what sits on it is the ordinary way to get
//! one, and a deep stack is where the ordering slips.
//!
//! Such a pull request cannot simply be reopened. GitHub refuses it while the
//! branch is missing, naming the branch and nothing else
//! (`GitHubRule::ReopeningNeedsTheRefsBack`) — the stack, which is still holding
//! the pull request, has nothing to do with the refusal. Nor can the base be
//! moved out of the way first: a closed pull request's base is frozen
//! (`GitHubRule::AClosedPullRequestsBaseCannotMove`), a refusal separate from
//! the stack's own lock on it and outliving that lock. Between the two, the
//! order is fixed and there is only one:
//!
//! 1. put the branch back,
//! 2. reopen,
//! 3. dissolve whatever stack holds the pull request, which is the only thing
//!    the stack ever blocked,
//! 4. retarget onto the base it should have had,
//! 5. take the branch away again.
//!
//! Steps 3 and 4 are the retargeting `diff` already does, and this module has
//! no opinion about them. What it owns is 1, 2 and 5.
//!
//! **Why step 1 is cheap.** A resurrected *base* ref is checked for existence
//! and not for identity — any commit at all reopens the pull request
//! (`GitHubRule::AResurrectedBaseRefNeedsNoParticularCommit`). Nothing has to be
//! fetched and nothing has to be remembered from before the deletion: the caller
//! names a commit it already has, and the only thing that commit decides is what
//! the pull request shows between step 2 and step 4. Both callers name the tip of
//! the master branch, which is the truthful choice for the case this is for — the
//! branch went away because what was on it landed, so what the pull request adds
//! to the master branch is what it adds to what its base became.
//!
//! A *head* ref is the other way round — it has to come back at the exact commit
//! the closed pull request records, or GitHub refuses the reopen for a branch
//! "force-pushed or recreated". That is why this module puts back base branches
//! only: a pull request whose own branch is gone is a different thing to have
//! lost, and the honest answer for it is to say so. See [`Reopenable`].

use crate::{
    error::{Error, Result, ResultExt},
    github::{GitHubApi, GitHubBranch, PullRequest, PullRequestState},
};

/// What a closed pull request is, as far as putting it back goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reopenable {
    /// Its base branch is not on the remote, which is the whole reason GitHub
    /// closed it. Putting the branch back and reopening puts the pull request
    /// back with it.
    ItsBaseBranchIsMissing,

    /// Its own branch is not on the remote. Reopening needs that branch back at
    /// the exact commit the pull request records, which is not a commit jj-spr
    /// can be sure it still has, and a branch put back at any other commit is
    /// refused. Reported rather than attempted.
    ItsOwnBranchIsMissing,

    /// Both refs are there, so the pull request is closed because somebody
    /// closed it. Reopening is their decision, not a repair.
    ItWasClosedOnPurpose,
}

/// Ask why pull request `pull_request` is closed.
///
/// Asked of the remote, one branch at a time, and only for a pull request that
/// is already known to be closed — three questions this never asks of a healthy
/// pull request, on a path that is not the common one.
///
/// The head branch is asked about first. A pull request can perfectly well have
/// lost both branches at once — deleting one branch in a chain closes the pull
/// request below it and the one above it, and a sweep that deletes several
/// closes a run of them — and of the two answers, the one that says a repair
/// will not work is the one worth having.
pub async fn diagnose(gh: &impl GitHubApi, pull_request: &PullRequest) -> Result<Reopenable> {
    debug_assert_eq!(pull_request.state, PullRequestState::Closed);

    if !gh.remote_branch_exists(&pull_request.head).await? {
        return Ok(Reopenable::ItsOwnBranchIsMissing);
    }

    if !gh.remote_branch_exists(&pull_request.base).await? {
        return Ok(Reopenable::ItsBaseBranchIsMissing);
    }

    Ok(Reopenable::ItWasClosedOnPurpose)
}

/// A branch put back so that a pull request could be reopened, which is the
/// repair's to take away again once the pull request is off it.
///
/// A value that has to be spent: [`Self::take_away`] consumes it, so a repair
/// that forgets to tidy up leaves an unused value behind rather than a branch
/// on the remote nobody remembers making.
#[must_use = "a scaffold branch is left on the remote until it is taken away"]
#[derive(Debug)]
pub struct Scaffold {
    branch: GitHubBranch,
    at: git2::Oid,
}

impl Scaffold {
    /// The branch this put back.
    pub fn branch(&self) -> &GitHubBranch {
        &self.branch
    }

    /// The commit the branch went back at.
    ///
    /// Worth keeping because a run that goes on to push its own commit to this
    /// same branch has to force past what the repair put there, and leasing that
    /// force to this commit is what keeps the force from reaching anything else.
    pub fn at(&self) -> git2::Oid {
        self.at
    }

    /// Leave the branch where it is.
    ///
    /// The counterpart to [`Self::take_away`], and named for the same reason
    /// the type is `#[must_use]`: a scaffold is either taken away or
    /// deliberately kept, and letting one fall out of scope unremarked is the
    /// mistake worth making impossible. `cleanup` keeps its scaffolds — the
    /// pull request it reopened is based on the branch and has nowhere else to
    /// go until something that reads the local chain retargets it.
    pub fn keep(self) {}

    /// Take the branch away again, reporting whether the remote still had it.
    ///
    /// Safe to call the moment the pull request has been retargeted and not
    /// before: deleting this branch is the very thing that closed the pull
    /// request in the first place, so it goes only once GitHub has confirmed
    /// that nothing points at it — which is what
    /// [`crate::github::GitHub::retarget_pull_request`] confirms before it
    /// deletes a base branch of its own for the same reason.
    pub async fn take_away(self, gh: &impl GitHubApi) -> Result<bool> {
        gh.delete_remote_branch(&self.branch).await
    }
}

/// Put pull request `number` back, restoring `base` at `scaffold_at` where the
/// remote no longer has it.
///
/// Hands back the scaffold branch where one was made, and `None` where the base
/// branch turned out to be on the remote after all — so a caller that reopens on
/// the strength of a [`diagnose`] from a moment ago cannot put a branch back over
/// one somebody has since restored, and gets a reopen and nothing else.
///
/// `scaffold_at` is free precisely because a base ref is checked for existence
/// only. What it decides is what the pull request shows until it is retargeted,
/// which is why both callers name the tip of the master branch rather than
/// anything they would have to go and find.
pub async fn put_back(
    gh: &impl GitHubApi,
    number: u64,
    base: &GitHubBranch,
    scaffold_at: git2::Oid,
) -> Result<Option<Scaffold>> {
    let scaffold = if gh.remote_branch_exists(base).await? {
        None
    } else {
        gh.create_remote_branch(base, scaffold_at).await?;

        Some(Scaffold {
            branch: base.clone(),
            at: scaffold_at,
        })
    };

    // A scaffold outlives a failed reopen deliberately, and the sentence says so
    // rather than leaving the branch to be found later and wondered about: it is
    // the one thing that makes another attempt possible, and taking it away here
    // would put the pull request back beyond reach.
    let reopened = gh.reopen_pull_request(number).await;

    match (reopened, scaffold) {
        (Ok(()), scaffold) => Ok(scaffold),
        (Err(error), None) => Err(error),
        (Err(error), Some(scaffold)) => {
            let context = format!(
                "Pull Request #{number} could not be reopened. {} has been put \
                 back on the remote and is being left there: without it the Pull \
                 Request cannot be reopened at all",
                scaffold.branch().branch_name()
            );
            scaffold.keep();

            Err(error).context(context)
        }
    }
}

/// What to tell somebody whose pull request cannot be put back.
///
/// Kept here rather than at the call sites so that `diff` and `cleanup` say the
/// same thing about the same situation. Both reach it having found a closed
/// pull request they were asked to work on, and the sentence has to leave the
/// reader with something to do.
pub fn cannot_be_put_back(number: u64, why: Reopenable) -> Error {
    match why {
        Reopenable::ItsOwnBranchIsMissing => Error::new(format!(
            "Pull Request #{number} is closed and its own branch is gone from the \
             remote, so GitHub will not reopen it: the branch would have to come \
             back at the exact commit the Pull Request records, and a branch put \
             back at any other commit is refused. Open a new Pull Request for the \
             change instead, by removing the 'Pull Request' section from its \
             commit message."
        )),
        Reopenable::ItWasClosedOnPurpose => Error::new(format!(
            "Pull Request #{number} is closed, and both of the branches it names \
             are on the remote — so it was closed deliberately rather than by a \
             branch going missing, and reopening it is not jj-spr's to decide. \
             Reopen it on GitHub, or open a new Pull Request for the change by \
             removing the 'Pull Request' section from its commit message."
        )),
        // Reachable only by a caller that asked this what to say about a pull
        // request it could have repaired. Spelled out rather than matched with a
        // wildcard so that a fourth variant has to come back here and say what it
        // means, and worded as what it is so that nobody reads it as advice.
        Reopenable::ItsBaseBranchIsMissing => Error::new(format!(
            "jj-spr gave up on Pull Request #{number}, whose base branch is missing \
             and which it could have put back. This is a bug in jj-spr."
        )),
    }
}
