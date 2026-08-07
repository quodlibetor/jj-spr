/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Moving the pull requests stacked on one that is leaving the stack, and
//! deciding which of the branches it leaves behind may go.
//!
//! `land` and `close` both take a pull request out of a stack and then want its
//! head branch gone. Under a linear `spr.baseStrategy` that branch is what the
//! pull requests above are based on, and GitHub closes a pull request whose
//! base branch is deleted, so they have to be pointed somewhere else first —
//! and the branch has to stay if any of them could not be.
//!
//! Where "somewhere else" is differs: `land` puts the changes on the master
//! branch, so the pull requests above belong there, while `close` puts them
//! nowhere, so they belong on the closed pull request's own base.
//!
//! How the callers find those pull requests differs too, and that difference
//! matters more: `close` asks GitHub which pull requests target the branch,
//! while `land` reads the local stack and so cannot see one whose change jj
//! does not have. Both hand what they found to
//! [`retarget_stacked_pull_requests`], which is why the head branch this module
//! agrees to delete is only as safe as the list it was given.

use std::process::Stdio;

use crate::{
    config::Config,
    error::{Error, Result},
    github::{GitHubBranch, StackedPullRequest},
    jj::Jujutsu,
    output::output,
};

/// Points a pull request at a new base branch, taking the old base branch out
/// of the way where it was ours to take.
///
/// A trait so that [`retarget_stacked_pull_requests`] can be exercised without
/// a GitHub to talk to; [`crate::github::GitHub`] is the only implementation
/// outside tests.
pub trait Retarget {
    /// See [`crate::github::GitHub::retarget_pull_request`], which is what this
    /// calls. Reports whether the old base branch was deleted from the remote.
    fn retarget(
        &self,
        number: u64,
        new_base: &GitHubBranch,
        old_base: &GitHubBranch,
    ) -> impl std::future::Future<Output = Result<bool>>;
}

impl Retarget for crate::github::GitHub {
    async fn retarget(
        &self,
        number: u64,
        new_base: &GitHubBranch,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        self.retarget_pull_request(number, new_base, old_base).await
    }
}

/// What came of retargeting the pull requests stacked on one.
#[derive(Debug, Default)]
pub struct Retargeted {
    /// The pull requests that now point at the new base branch.
    pub moved: Vec<u64>,
    /// Whether a retarget that reported failure was moving a pull request off
    /// the head branch of the pull request leaving the stack.
    ///
    /// Only what GitHub *reported* is known: a retarget can fail after the base
    /// was already changed, so this says the pull request may still be based on
    /// the head branch, not that it is.
    failed_off_head_branch: bool,
}

impl Retargeted {
    /// Whether the head branch of the pull request leaving the stack may be
    /// deleted now.
    ///
    /// It may not while a pull request may still be based on it: the branch
    /// going away would close that pull request, which is worth far more than
    /// the branch left behind.
    ///
    /// A retarget that reported failure keeps the branch. GitHub said the move
    /// did not happen, and a failure raised *after* the base was already
    /// changed cannot be told apart from one raised before — so the pull
    /// request is treated as still there, which is the side that only costs a
    /// branch when it is wrong.
    pub fn may_delete_head_branch(&self) -> bool {
        !self.failed_off_head_branch
    }
}

/// Point every pull request in `stacked` at `new_base`, reporting each outcome.
///
/// `head_branch` is the head branch of the pull request leaving the stack, the
/// one the caller wants to delete afterwards. A pull request that could not be
/// moved off it makes [`Retargeted::may_delete_head_branch`] say no.
///
/// Fails without touching GitHub when `new_base` and `head_branch` name the
/// same branch, which is the shape a caller that passes the two same-typed
/// arguments the wrong way round takes: retargeting every pull request onto the
/// branch that is about to be deleted would report success and then close them
/// all. No real caller can hit it — a pull request never targets its own head
/// branch, and a head branch is never the master branch.
///
/// Retargeting is otherwise best-effort: what brought us here — the land or the
/// close — has already happened, so a pull request that cannot be moved is
/// reported rather than failing the command.
pub async fn retarget_stacked_pull_requests(
    gh: &impl Retarget,
    stacked: &[StackedPullRequest],
    new_base: &GitHubBranch,
    head_branch: &GitHubBranch,
) -> Result<Retargeted> {
    if new_base.branch_name() == head_branch.branch_name() {
        return Err(Error::new(format!(
            "Refusing to retarget Pull Requests onto '{}', the very branch \
             they are being moved off. This is a bug in jj-spr.",
            new_base.branch_name()
        )));
    }

    let mut retargeted = Retargeted::default();

    for pull_request in stacked {
        match gh
            .retarget(pull_request.number, new_base, &pull_request.base)
            .await
        {
            Ok(deleted) => {
                output(
                    "🎯",
                    &format!(
                        "Retargeted Pull Request #{} to {}",
                        pull_request.number,
                        new_base.branch_name()
                    ),
                )?;

                if deleted {
                    output(
                        "🗑️",
                        &format!("Deleted {}", pull_request.base.branch_name()),
                    )?;
                }

                retargeted.moved.push(pull_request.number);
            }
            Err(error) => {
                retargeted.failed_off_head_branch |=
                    pull_request.base.branch_name() == head_branch.branch_name();

                output(
                    "⚠️",
                    &format!(
                        "Could not retarget Pull Request #{} to {}",
                        pull_request.number,
                        new_base.branch_name()
                    ),
                )?;
                for message in error.messages() {
                    output("  ", message)?;
                }
            }
        }
    }

    Ok(retargeted)
}

/// Whether the base branch of the pull request leaving the stack is that pull
/// request's to delete.
///
/// Ownership answers this, not `spr.baseStrategy`: one repository can hold pull
/// requests made under either strategy at once, so what the setting says today
/// says nothing about the branch in front of us. What the branch name says
/// covers all of them:
///
/// - under `synthetic`, and where `linear` fell back to a generated base branch
///   for this change, the base branch was generated for this pull request alone
///   and nothing else wants it;
/// - under `linear` the base is the head branch of the pull request below, and
///   deleting it would close *that* pull request;
/// - a base branch set by hand — a release branch, a colleague's branch — was
///   never jj-spr's to take away.
///
/// Even a generated base branch stays while a pull request is based on it.
/// Ownership says there should be none — [`Config::get_base_branch_name`] picks
/// a name no other branch has — but `close` retargets the pull requests above
/// onto this very branch, so it is the one thing that can put a second pull
/// request there, and deleting the branch would close it.
///
/// `based_on_base` is what the caller found to be based on `base`, and the
/// caller decides how hard it looked: `close` asks GitHub after retargeting,
/// which is the only answer that covers a pull request it did not move itself.
/// `land` passes an empty list — it retargets onto the master branch, so it
/// aims nothing here, and it does not ask whether anything else did.
///
/// `None` means the caller tried to find out and could not. The branch stays:
/// not knowing what a deletion would close is exactly the situation to leave a
/// branch behind in. A caller may pass `None` unasked when
/// [`base_branch_is_ours`] is false, since the answer is then `false` either
/// way and the lookup would be wasted.
pub fn may_delete_base_branch(
    config: &Config,
    base: &GitHubBranch,
    based_on_base: Option<&[StackedPullRequest]>,
) -> bool {
    based_on_base.is_some_and(<[_]>::is_empty) && base_branch_is_ours(config, base)
}

/// Whether `base` is a base branch jj-spr generated, and so one the pull
/// request that points at it could take away.
///
/// The ownership half of [`may_delete_base_branch`], named so that a caller
/// working out whether it is worth asking GitHub what is based on the branch
/// asks the same question the answer will be judged against. Were the two to
/// drift apart, a caller could skip that lookup for a branch that is then
/// deleted out from under whatever the lookup would have found.
pub fn base_branch_is_ours(config: &Config, base: &GitHubBranch) -> bool {
    config.is_synthetic_base_branch(base.branch_name())
}

/// Start deleting `branch` from the remote.
///
/// The deletion runs in the background so the caller can get on with its own
/// work; the caller waits on the returned child. Its result is not worth
/// looking at: GitHub may be configured to delete the branch itself, in which
/// case it is already gone and the push fails.
pub fn spawn_branch_deletion(
    jj: &Jujutsu,
    config: &Config,
    branch: &GitHubBranch,
) -> Result<tokio::process::Child> {
    Ok(jj
        .git_command()
        .arg("push")
        .arg("--no-verify")
        .arg("--delete")
        .arg("--")
        .arg(&config.remote_name)
        .arg(branch.on_github())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

/// Start deleting `head_branch` from the remote, or say why it is being kept.
///
/// Only once the pull requests above have been retargeted may the head branch
/// of the one leaving the stack go: under a linear `spr.baseStrategy` it is
/// what they were based on, and GitHub closes a pull request whose base branch
/// disappears. One that could not be retargeted keeps the branch alive — what
/// brought us here is done either way, and a branch left behind is worth far
/// less than a pull request closed out from under its reviewer.
pub fn spawn_head_branch_deletion(
    jj: &Jujutsu,
    config: &Config,
    head_branch: &GitHubBranch,
    retargeted: &Retargeted,
) -> Result<Option<tokio::process::Child>> {
    if !retargeted.may_delete_head_branch() {
        output(
            "🌱",
            &format!(
                "Kept {}: a Pull Request that could not be retargeted is still \
                 based on it. Retarget it and delete the branch, or run \
                 `jj spr diff` on that change.",
                head_branch.branch_name()
            ),
        )?;

        return Ok(None);
    }

    Ok(Some(spawn_branch_deletion(jj, config, head_branch)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn branch(name: &str) -> GitHubBranch {
        GitHubBranch::new_from_branch_name(name, "origin", "master")
    }

    fn config() -> Config {
        Config::new(
            "acme".into(),
            "codez".into(),
            "origin".into(),
            "master".into(),
            "spr/foo/".into(),
            false,
        )
    }

    /// A generated base branch is the pull request's own, so it goes. Both
    /// `spr.baseStrategy = synthetic` and the fallback `linear` takes when it
    /// cannot base a change on the one below it — no parent in the run, a
    /// cherry-pick — produce this one branch shape, which is exactly why
    /// ownership can be read off the name without consulting the setting.
    #[test]
    fn a_generated_base_branch_is_deleted() {
        assert!(may_delete_base_branch(
            &config(),
            &branch("spr/foo/master.my-feature"),
            Some(&[])
        ));
    }

    /// Under a linear `spr.baseStrategy` the base branch is the head branch of
    /// the pull request below. Deleting it would close that pull request.
    #[test]
    fn the_head_branch_below_is_not_deleted() {
        assert!(!may_delete_base_branch(
            &config(),
            &branch("spr/foo/my-feature"),
            Some(&[])
        ));
    }

    /// Even a generated base branch stays once pull requests have been aimed at
    /// it, which is what `close` does with the ones stacked on the pull request
    /// it closes.
    #[test]
    fn a_base_branch_holding_up_a_retargeted_pull_request_is_kept() {
        let stacked = vec![StackedPullRequest {
            number: 7,
            base: branch("spr/foo/my-feature"),
        }];

        assert!(!may_delete_base_branch(
            &config(),
            &branch("spr/foo/master.my-feature"),
            Some(&stacked),
        ));
    }

    /// A lookup that could not be made keeps the branch: not knowing what
    /// deleting it would close is the situation to leave it behind in.
    #[test]
    fn a_base_branch_nobody_could_ask_about_is_kept() {
        assert!(!may_delete_base_branch(
            &config(),
            &branch("spr/foo/master.my-feature"),
            None
        ));
    }

    /// A base branch nobody generated is nobody's to delete. `close` used to
    /// spare only the master branch, so a release branch someone set by hand
    /// went the same way as a generated one.
    #[test]
    fn a_hand_set_base_branch_is_not_deleted() {
        for name in ["master", "release-1.0", "a-colleagues-branch"] {
            assert!(
                !may_delete_base_branch(&config(), &branch(name), Some(&[])),
                "{name}"
            );
        }
    }

    /// One call [`retarget_stacked_pull_requests`] made, as the fake below saw
    /// it: which pull request, and the branch names it was moved from and to.
    #[derive(Debug, PartialEq, Eq)]
    struct Call {
        number: u64,
        new_base: String,
        old_base: String,
    }

    /// A [`Retarget`] that records what it was asked and answers from a script.
    struct FakeGitHub {
        /// What to answer, in call order: `Ok(deleted)` or an error message.
        answers: RefCell<Vec<std::result::Result<bool, String>>>,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeGitHub {
        fn new(answers: Vec<std::result::Result<bool, String>>) -> Self {
            Self {
                answers: RefCell::new(answers),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Retarget for FakeGitHub {
        async fn retarget(
            &self,
            number: u64,
            new_base: &GitHubBranch,
            old_base: &GitHubBranch,
        ) -> Result<bool> {
            self.calls.borrow_mut().push(Call {
                number,
                new_base: new_base.branch_name().to_string(),
                old_base: old_base.branch_name().to_string(),
            });

            match self.answers.borrow_mut().remove(0) {
                Ok(deleted) => Ok(deleted),
                Err(message) => Err(Error::new(message)),
            }
        }
    }

    fn stacked_on(head_branch: &GitHubBranch, numbers: &[u64]) -> Vec<StackedPullRequest> {
        numbers
            .iter()
            .map(|number| StackedPullRequest {
                number: *number,
                base: head_branch.clone(),
            })
            .collect()
    }

    /// Every stacked pull request is moved to the base the caller names, not to
    /// the master branch: `close` puts nothing on master, so a pull request
    /// sent there would absorb the changes of every pull request below it.
    #[tokio::test]
    async fn test_retargets_to_the_base_the_caller_names() {
        let head = branch("spr/a-change");
        let new_base = branch("spr/the-change-below");
        let gh = FakeGitHub::new(vec![Ok(false), Ok(false)]);

        let retargeted =
            retarget_stacked_pull_requests(&gh, &stacked_on(&head, &[7, 8]), &new_base, &head)
                .await
                .unwrap();

        assert_eq!(
            *gh.calls.borrow(),
            vec![
                Call {
                    number: 7,
                    new_base: "spr/the-change-below".to_string(),
                    old_base: "spr/a-change".to_string(),
                },
                Call {
                    number: 8,
                    new_base: "spr/the-change-below".to_string(),
                    old_base: "spr/a-change".to_string(),
                },
            ]
        );
        assert_eq!(retargeted.moved, vec![7, 8]);
        assert!(retargeted.may_delete_head_branch());
    }

    /// Retargeting onto the very branch being deleted would report success and
    /// then close everything above, so it is refused before GitHub is touched.
    /// The two arguments are the same type, so nothing else catches a caller
    /// that passes them the wrong way round.
    #[tokio::test]
    async fn test_retargeting_onto_the_head_branch_is_refused() {
        let head = branch("spr/a-change");
        let gh = FakeGitHub::new(vec![Ok(false)]);

        let result =
            retarget_stacked_pull_requests(&gh, &stacked_on(&head, &[7]), &head, &head).await;

        assert!(result.is_err());
        assert!(
            gh.calls.borrow().is_empty(),
            "GitHub must not be touched at all"
        );
    }

    /// A pull request that could not be moved off the head branch keeps that
    /// branch alive, even though the others were moved.
    #[tokio::test]
    async fn test_a_failed_retarget_keeps_the_head_branch() {
        let head = branch("spr/a-change");
        let new_base = branch("master");
        let gh = FakeGitHub::new(vec![Ok(true), Err("GitHub said no".to_string())]);

        let retargeted =
            retarget_stacked_pull_requests(&gh, &stacked_on(&head, &[7, 8]), &new_base, &head)
                .await
                .unwrap();

        assert_eq!(retargeted.moved, vec![7]);
        assert!(!retargeted.may_delete_head_branch());
    }

    /// A pull request that failed to retarget away from something other than
    /// the head branch — a synthetic base branch, as under
    /// `spr.baseStrategy = synthetic` — leaves the head branch deletable: it
    /// was never based on it, so deleting it cannot close anything.
    #[tokio::test]
    async fn test_a_failed_retarget_off_another_branch_frees_the_head_branch() {
        let head = branch("spr/a-change");
        let new_base = branch("master");
        let stacked = vec![StackedPullRequest {
            number: 7,
            base: branch("spr/master.a-change"),
        }];
        let gh = FakeGitHub::new(vec![Err("GitHub said no".to_string())]);

        let retargeted = retarget_stacked_pull_requests(&gh, &stacked, &new_base, &head)
            .await
            .unwrap();

        assert!(retargeted.moved.is_empty());
        assert!(retargeted.may_delete_head_branch());
    }

    /// Nothing stacked on the pull request means nothing in the way of its head
    /// branch.
    #[tokio::test]
    async fn test_nothing_stacked_frees_the_head_branch() {
        let head = branch("spr/a-change");
        let gh = FakeGitHub::new(Vec::new());

        let retargeted = retarget_stacked_pull_requests(&gh, &[], &branch("master"), &head)
            .await
            .unwrap();

        assert!(retargeted.moved.is_empty());
        assert!(retargeted.may_delete_head_branch());
    }
}
