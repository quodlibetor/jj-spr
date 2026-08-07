/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The GitHub operations jj-spr's commands perform, as a trait, so that a test
//! can stand something else in GitHub's place.
//!
//! [`GitHub`] is the only implementation outside tests. The point of the trait is
//! the other one: a fake that keeps pull requests and stacks in memory while the
//! branches go to a bare repository on disk, which lets everything `diff`, `land`
//! and `close` *decide* be tested without a network — see
//! `spr/tests/fake_github_test.rs`.
//!
//! **A fake is only as honest as what pins it.** Every rule a fake has to
//! reproduce — that a stack refuses a base change, that only a generated base
//! branch is deleted when a pull request leaves it — is GitHub's rule, learnt
//! from GitHub and true only as long as GitHub says so. Those live in the
//! end-to-end suite, which is what a fake cannot replace: a test against a fake
//! asserts what jj-spr does, and only a test against GitHub asserts what that
//! does to GitHub. So the two suites divide as follows, and adding to the fake
//! means asking which side of the line the new behaviour is on:
//!
//! - **jj-spr's own doing** — which base a pull request is given, what its branch
//!   is built out of, which calls are made in which order, what is refused
//!   outright: fast tests against the fake.
//! - **GitHub's doing** — what a retarget does to a pull request, what its stack
//!   merge does to the ones above, what it makes of a branch: the end-to-end
//!   suite, which is also where the fake's rules are checked.
//!
//! Which rule is pinned by which live test is not left to prose: every one of
//! them is a variant of [`crate::github::GitHubRule`], and the contract suite
//! cannot compile unless each variant names the test that pins it.
//!
//! Deliberately not the whole of [`GitHub`]. `list` and `cleanup` still take it
//! concretely, and the associated functions for looking users and teams up are
//! left out as well: they are reached only for a change whose message names
//! reviewers, so a test that names none never calls them.

use std::future::Future;

use super::{
    AsyncMerge, GitHub, MergeQueue, MergeQueueEntry, PullRequest, PullRequestMergeability,
    PullRequestRequestReviewers, PullRequestUpdate, QueuedPullRequest, Stack, StackResult,
    StackedPullRequest, UnstackOutcome,
};
use crate::{error::Result, github::GitHubBranch, message::MessageSectionsMap};

/// A rule of GitHub's that jj-spr is built on and a fake GitHub has to
/// reproduce.
///
/// This is the contract between the two test suites, written down so that it
/// cannot quietly rot. The fast tests run against a fake that behaves like this;
/// each variant is a claim about the real GitHub that only a live test can
/// establish, and `spr/tests/github_e2e_test.rs` names the test that establishes
/// each one in a total `match` — so adding a rule here does not compile until a
/// live test is named for it, and renaming that test fails the meta-test that
/// checks the names against the file.
///
/// Every rule was established by probing the live API, not read out of
/// documentation, which describes almost none of it. Where a jj-spr behaviour
/// exists *because* of a rule, the code says which — the point of naming them is
/// that "why does jj-spr bother doing this?" has an answer that can be re-checked
/// against GitHub in one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubRule {
    /// A stack owns its members' base refs: any update carrying a `base` is
    /// refused while a stack holds the pull request, whether or not the value
    /// differs. This is why anything that moves a base dissolves the stack first.
    StackLocksBaseRefs,

    /// The ordinary merge endpoint refuses a pull request a stack holds, and
    /// points at the asynchronous one instead. This is why landing dissolves the
    /// stack even when it moves no base at all.
    MergingAStackedPullRequestNeedsTheAsyncEndpoint,

    /// The ordinary merge is leased to the head commit it was given: a pull
    /// request pushed to since is refused rather than merged unseen.
    MergeIsLeasedToTheHead,

    /// Deleting a branch closes every open pull request based on it. This is why
    /// a branch is only ever deleted after the pull requests on it have been
    /// retargeted, and why a retarget that failed keeps the branch.
    DeletingABaseBranchClosesItsPullRequests,

    /// A stack's members must chain base-to-head, bottom first, or the stacks API
    /// refuses to make one.
    StackMembersMustChain,

    /// Unstacking a stack whose members are all unmerged releases them and
    /// destroys the stack record itself.
    UnstackReleasesUnmergedMembers,

    /// The asynchronous merge merges the pull request it is given *and every
    /// member of its stack below it*, bottom first, one commit each. There is no
    /// asking for less.
    AsyncMergeMergesDownwards,

    /// After such a merge, GitHub retargets the members above onto the stack's
    /// base and force-pushes their branches onto it, keeping the stack. This is
    /// the rule that makes the stack merge safe under
    /// [`BaseStrategy::LinearRebase`](crate::config::BaseStrategy::LinearRebase)
    /// and destructive under the other two.
    AsyncMergeRebasesTheSurvivors,

    /// A force-pushed head branch leaves its pull request open, and its diff is
    /// recomputed from the new merge base. This is what makes
    /// [`BaseStrategy::LinearRebase`](crate::config::BaseStrategy::LinearRebase)
    /// possible at all.
    AForcePushKeepsThePullRequestOpen,

    /// A squash merge puts exactly one commit on the base branch, carrying the
    /// whole of the pull request.
    SquashMergeLandsOneCommit,
}

impl GitHubRule {
    /// Every rule, so that a test can walk them.
    pub const ALL: [GitHubRule; 10] = [
        Self::StackLocksBaseRefs,
        Self::MergingAStackedPullRequestNeedsTheAsyncEndpoint,
        Self::MergeIsLeasedToTheHead,
        Self::DeletingABaseBranchClosesItsPullRequests,
        Self::StackMembersMustChain,
        Self::UnstackReleasesUnmergedMembers,
        Self::AsyncMergeMergesDownwards,
        Self::AsyncMergeRebasesTheSurvivors,
        Self::AForcePushKeepsThePullRequestOpen,
        Self::SquashMergeLandsOneCommit,
    ];
}

/// What `diff`, `land` and `close` ask of GitHub.
///
/// The signatures are the ones [`GitHub`] already had, so that the
/// implementation below is delegation and nothing else — anything this trait
/// smoothed over would be a difference between what the tests exercise and what
/// runs.
pub trait GitHubApi {
    /// See [`GitHub::get_pull_request`].
    fn get_pull_request(&self, number: u64) -> impl Future<Output = Result<PullRequest>>;

    /// See [`GitHub::create_pull_request`].
    fn create_pull_request(
        &self,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> impl Future<Output = Result<u64>>;

    /// See [`GitHub::update_pull_request`].
    fn update_pull_request(
        &self,
        number: u64,
        updates: PullRequestUpdate,
    ) -> impl Future<Output = Result<()>>;

    /// See [`GitHub::retarget_pull_request`].
    fn retarget_pull_request(
        &self,
        number: u64,
        new_base: &GitHubBranch,
        old_base: &GitHubBranch,
    ) -> impl Future<Output = Result<bool>>;

    /// See [`GitHub::retarget_to_master_branch`].
    fn retarget_to_master_branch(
        &self,
        number: u64,
        old_base: &GitHubBranch,
    ) -> impl Future<Output = Result<bool>>;

    /// See [`GitHub::request_reviewers`].
    fn request_reviewers(
        &self,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> impl Future<Output = Result<()>>;

    /// See [`GitHub::get_open_stack_for_pull_request`].
    fn get_open_stack_for_pull_request(
        &self,
        number: u64,
    ) -> impl Future<Output = StackResult<Option<Stack>>>;

    /// See [`GitHub::create_stack`].
    fn create_stack(&self, pull_requests: &[u64]) -> impl Future<Output = StackResult<Stack>>;

    /// See [`GitHub::add_to_stack`].
    fn add_to_stack(
        &self,
        stack_number: u64,
        pull_requests: &[u64],
    ) -> impl Future<Output = StackResult<Stack>>;

    /// See [`GitHub::unstack`].
    fn unstack(&self, stack_number: u64) -> impl Future<Output = StackResult<UnstackOutcome>>;

    /// See [`GitHub::get_stack`].
    fn get_stack(&self, stack_number: u64) -> impl Future<Output = StackResult<Stack>>;

    /// See [`GitHub::get_pull_requests_with_base`].
    fn get_pull_requests_with_base(
        &self,
        base: &GitHubBranch,
    ) -> impl Future<Output = Result<Vec<StackedPullRequest>>>;

    /// See [`GitHub::get_pull_request_mergeability`].
    fn get_pull_request_mergeability(
        &self,
        number: u64,
    ) -> impl Future<Output = Result<PullRequestMergeability>>;

    /// See [`GitHub::get_merge_queue`].
    fn get_merge_queue(
        &self,
        branch_name: &str,
    ) -> impl Future<Output = Result<Option<MergeQueue>>>;

    /// See [`GitHub::enqueue_pull_request`].
    fn enqueue_pull_request(
        &self,
        node_id: &str,
        head_oid: git2::Oid,
    ) -> impl Future<Output = Result<MergeQueueEntry>>;

    /// See [`GitHub::get_queued_pull_request`].
    fn get_queued_pull_request(
        &self,
        number: u64,
    ) -> impl Future<Output = Result<QueuedPullRequest>>;

    /// See [`GitHub::merge_pull_request`].
    fn merge_pull_request(
        &self,
        number: u64,
        title: String,
        message: String,
        head_oid: git2::Oid,
    ) -> impl Future<Output = Result<Option<git2::Oid>>>;

    /// See [`GitHub::merge_pull_request_async`].
    fn merge_pull_request_async(
        &self,
        number: u64,
    ) -> impl Future<Output = StackResult<AsyncMerge>>;
}

impl GitHubApi for GitHub {
    /// The one method that is not plain delegation: the inherent one consumes a
    /// [`GitHub`], which every caller satisfies by cloning, so the clone happens
    /// here instead.
    async fn get_pull_request(&self, number: u64) -> Result<PullRequest> {
        self.clone().get_pull_request(number).await
    }

    async fn create_pull_request(
        &self,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64> {
        GitHub::create_pull_request(self, message, base_ref_name, head_ref_name, draft).await
    }

    async fn update_pull_request(&self, number: u64, updates: PullRequestUpdate) -> Result<()> {
        GitHub::update_pull_request(self, number, updates).await
    }

    async fn retarget_pull_request(
        &self,
        number: u64,
        new_base: &GitHubBranch,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        GitHub::retarget_pull_request(self, number, new_base, old_base).await
    }

    async fn retarget_to_master_branch(
        &self,
        number: u64,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        GitHub::retarget_to_master_branch(self, number, old_base).await
    }

    async fn request_reviewers(
        &self,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> Result<()> {
        GitHub::request_reviewers(self, number, reviewers).await
    }

    async fn get_open_stack_for_pull_request(&self, number: u64) -> StackResult<Option<Stack>> {
        GitHub::get_open_stack_for_pull_request(self, number).await
    }

    async fn create_stack(&self, pull_requests: &[u64]) -> StackResult<Stack> {
        GitHub::create_stack(self, pull_requests).await
    }

    async fn add_to_stack(&self, stack_number: u64, pull_requests: &[u64]) -> StackResult<Stack> {
        GitHub::add_to_stack(self, stack_number, pull_requests).await
    }

    async fn unstack(&self, stack_number: u64) -> StackResult<UnstackOutcome> {
        GitHub::unstack(self, stack_number).await
    }

    async fn get_stack(&self, stack_number: u64) -> StackResult<Stack> {
        GitHub::get_stack(self, stack_number).await
    }

    async fn get_pull_requests_with_base(
        &self,
        base: &GitHubBranch,
    ) -> Result<Vec<StackedPullRequest>> {
        GitHub::get_pull_requests_with_base(self, base).await
    }

    async fn get_pull_request_mergeability(&self, number: u64) -> Result<PullRequestMergeability> {
        GitHub::get_pull_request_mergeability(self, number).await
    }

    async fn get_merge_queue(&self, branch_name: &str) -> Result<Option<MergeQueue>> {
        GitHub::get_merge_queue(self, branch_name).await
    }

    async fn enqueue_pull_request(
        &self,
        node_id: &str,
        head_oid: git2::Oid,
    ) -> Result<MergeQueueEntry> {
        GitHub::enqueue_pull_request(self, node_id, head_oid).await
    }

    async fn get_queued_pull_request(&self, number: u64) -> Result<QueuedPullRequest> {
        GitHub::get_queued_pull_request(self, number).await
    }

    async fn merge_pull_request(
        &self,
        number: u64,
        title: String,
        message: String,
        head_oid: git2::Oid,
    ) -> Result<Option<git2::Oid>> {
        GitHub::merge_pull_request(self, number, title, message, head_oid).await
    }

    async fn merge_pull_request_async(&self, number: u64) -> StackResult<AsyncMerge> {
        GitHub::merge_pull_request_async(self, number).await
    }
}
