/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The changes around the ones a run was given, and how far out they reach.
//!
//! A linear `spr.baseStrategy` bases each pull request on the head branch of the
//! change below it. That is a fact about the *local chain*, not about the
//! revisions a run was handed: a change added on top of a stack is stacked on it
//! whether or not the run that pushes it also pushes what is underneath. So a
//! run takes in its neighbours, in two different senses:
//!
//! - **Below**, as far as the pull requests reach, is *context*. Nothing there
//!   is pushed. It supplies the base branch for the bottom of the run and the
//!   lower half of the chain that GitHub is told is a stack, which is what makes
//!   a change pushed on its own join the stack it sits on rather than start an
//!   island of its own.
//! - **Above** is *work*. A run that moves a base or rewrites a branch moves the
//!   ground under everything stacked on it, so the descendants are pushed too
//!   and are re-measured against what this run just did. Each one decides for
//!   itself whether it has anything to do, so taking one in costs nothing where
//!   nothing moved.
//!
//! Only changes that already have a pull request are taken in, in either
//! direction. A change without one is where the neighbourhood stops: reaching
//! past it would mean opening a pull request nobody asked for, and nothing above
//! it could name what it is based on anyway.
//!
//! These are plain walks over commits jj has already prepared, so that what a
//! run reaches is decided in one readable place and can be tested without a
//! repository. Fetching the commits and their pull requests is
//! [`crate::commands::diff`]'s business.

use std::collections::HashMap;

use git2::Oid;

use crate::jj::PreparedCommit;

/// The descendants of `top_oid` that the run should push as well, bottom-up.
///
/// `descendants` is every local descendant, in any order — the walk follows
/// parent links rather than trusting the order it was given.
///
/// The walk stops at the first change that is not an unambiguous continuation of
/// the one below it:
///
/// - **a change with no pull request**, which is the usual end of a stack: the
///   working copy is an empty change on top of the one being worked on, and a
///   run must not open a pull request for it;
/// - **a fork**, where one change has two children. Neither is "the change
///   above" and pushing both would be two stacks, not one, so a run that was
///   given one revision does not get to guess which the user meant.
pub fn descendants_to_push(top_oid: Oid, descendants: Vec<PreparedCommit>) -> Vec<PreparedCommit> {
    let mut children: HashMap<Oid, Vec<PreparedCommit>> = HashMap::new();
    for commit in descendants {
        children.entry(commit.parent_oid).or_default().push(commit);
    }

    let mut chain = Vec::new();
    let mut current = top_oid;

    while let Some(mut kids) = children.remove(&current) {
        if kids.len() != 1 {
            break;
        }

        let child = kids.pop().expect("a single child");
        if child.pull_request_number.is_none() {
            break;
        }

        current = child.oid;
        chain.push(child);
    }

    chain
}

/// The run of changes below `parent_oid` — that change included — that have pull
/// requests, bottom-up.
///
/// `ancestors` is the commits between the master branch and `parent_oid`, in any
/// order. The walk goes downwards from `parent_oid` and stops at the first
/// change without a pull request, so what comes back is the unbroken run
/// adjacent to the run's own bottom rather than every pull request underneath
/// it. A gap below is somebody else's stack: this run neither builds on it nor
/// touches it.
///
/// An empty answer means the bottom of the run has nothing to be based on, which
/// is what it already had to cope with.
pub fn ancestors_below(parent_oid: Oid, ancestors: Vec<PreparedCommit>) -> Vec<PreparedCommit> {
    let mut by_oid: HashMap<Oid, PreparedCommit> =
        ancestors.into_iter().map(|c| (c.oid, c)).collect();

    let mut chain = Vec::new();
    let mut current = parent_oid;

    // `remove` rather than `get`: a commit that is somehow its own parent — the
    // root commit, as `prepare_commit` describes it — would otherwise be walked
    // forever.
    while let Some(commit) = by_oid.remove(&current) {
        if commit.pull_request_number.is_none() {
            break;
        }

        current = commit.parent_oid;
        chain.push(commit);
    }

    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageSection, MessageSectionsMap};

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).expect("an object id")
    }

    /// A prepared commit with the given identity, and a pull request where
    /// `pull_request_number` says so.
    fn commit(id: u8, parent: u8, pull_request_number: Option<u64>) -> PreparedCommit {
        PreparedCommit {
            oid: oid(id),
            short_id: format!("{id:02x}"),
            parent_oid: oid(parent),
            message: MessageSectionsMap::from([(MessageSection::Title, format!("change {id}"))]),
            pull_request_number,
            message_changed: false,
            dry_run_action: None,
            dry_run_stack_change: None,
        }
    }

    fn ids(commits: &[PreparedCommit]) -> Vec<Oid> {
        commits.iter().map(|commit| commit.oid).collect()
    }

    #[test]
    fn the_descendants_above_a_change_are_taken_in_bottom_up() {
        let taken = descendants_to_push(oid(1), vec![commit(3, 2, Some(3)), commit(2, 1, Some(2))]);

        assert_eq!(ids(&taken), vec![oid(2), oid(3)]);
    }

    #[test]
    fn a_change_with_no_pull_request_ends_the_descendants() {
        let taken = descendants_to_push(
            oid(1),
            vec![
                commit(2, 1, Some(2)),
                commit(3, 2, None),
                commit(4, 3, Some(4)),
            ],
        );

        assert_eq!(
            ids(&taken),
            vec![oid(2)],
            "nothing above the change with no pull request should come along, since a run \
             must not open one for it"
        );
    }

    #[test]
    fn a_fork_ends_the_descendants() {
        let taken = descendants_to_push(oid(1), vec![commit(2, 1, Some(2)), commit(3, 1, Some(3))]);

        assert!(
            taken.is_empty(),
            "neither branch of a fork is `the change above`, so a run should take in neither"
        );
    }

    #[test]
    fn there_is_nothing_above_a_change_with_no_descendants() {
        assert!(descendants_to_push(oid(1), Vec::new()).is_empty());
    }

    #[test]
    fn the_ancestors_below_a_change_are_taken_in_bottom_up() {
        let taken = ancestors_below(oid(3), vec![commit(2, 1, Some(2)), commit(3, 2, Some(3))]);

        assert_eq!(ids(&taken), vec![oid(2), oid(3)]);
    }

    #[test]
    fn a_change_with_no_pull_request_ends_the_ancestors() {
        let taken = ancestors_below(
            oid(4),
            vec![
                commit(2, 1, Some(2)),
                commit(3, 2, None),
                commit(4, 3, Some(4)),
            ],
        );

        assert_eq!(
            ids(&taken),
            vec![oid(4)],
            "the chain stops at the gap: what is below it is another stack, not this one"
        );
    }

    #[test]
    fn a_parent_with_no_pull_request_leaves_nothing_below() {
        assert!(ancestors_below(oid(2), vec![commit(2, 1, None)]).is_empty());
    }

    #[test]
    fn a_root_commit_that_is_its_own_parent_does_not_loop() {
        assert_eq!(
            ids(&ancestors_below(oid(1), vec![commit(1, 1, Some(1))])),
            vec![oid(1)]
        );
    }
}
