/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The GitHub operations `jj spr diff` performs, as a trait, so that a test can
//! stand something else in GitHub's place.
//!
//! [`GitHub`] is the only implementation outside tests. The point of the trait is
//! the other one: a fake that keeps pull requests and stacks in memory while the
//! branches go to a bare repository on disk, which lets everything `diff`
//! *decides* be tested without a network — see
//! `spr/tests/fake_github_diff_test.rs`.
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
//! Deliberately not the whole of [`GitHub`]. `land` and `close` still take it
//! concretely, and the associated functions for looking users and teams up are
//! left out as well: they are reached only for a change whose message names
//! reviewers, so a test that names none never calls them.

use std::future::Future;

use super::{
    GitHub, PullRequest, PullRequestRequestReviewers, PullRequestUpdate, Stack, StackResult,
    UnstackOutcome,
};
use crate::{error::Result, github::GitHubBranch, message::MessageSectionsMap};

/// What `diff` asks of GitHub.
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
}
