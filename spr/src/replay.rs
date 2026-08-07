/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rebuilding a pull request branch as the change's own commits sitting on a
//! new base, with no merge commit anywhere.
//!
//! This is what [`BaseStrategy::LinearRebase`](crate::config::BaseStrategy::LinearRebase)
//! does instead of merging the new base into what the branch already carries.
//! The two strategies want the same thing of a branch — that the difference
//! between it and its base is exactly the change under review — and reach it
//! from opposite directions: a merge keeps every commit that was ever pushed
//! and lets the graph carry the base along, while a replay keeps the *shape*
//! and rewrites the commits. Only the second survives GitHub rebasing the
//! branch, which its stacked pull requests do to the pull request above the one
//! they merge.
//!
//! Two things follow from rewriting rather than merging, and both are the
//! caller's to handle: the push has to be forced, and the commits a reviewer
//! has already looked at come back with different ids. What is kept is their
//! number and their content — one commit per round of the change, each with its
//! message, author and timestamps — so a branch that was three commits of
//! review history is three commits afterwards, rebased.
//!
//! Nothing here talks to GitHub or to jj: a branch is an `Oid` its commits hang
//! from, so all of it is decided in the object database.

use git2::Oid;

use crate::error::{Error, Result};

/// How far back the walk for a branch's own commits will go before giving up.
///
/// The walk stops at the base it was given, so this only bounds the work when
/// that base is not on the branch at all — a shape [`own_commits`] rejects
/// anyway, but only once it has looked. The number itself is arbitrary and
/// generous: a change pushed this many times has more history than is worth
/// replaying, and one that has not is nowhere near it.
const WALK_LIMIT: usize = 500;

/// What a rebuild made of the pull request branch, before the change's current
/// tree is added on top of it.
#[derive(Debug)]
pub struct Rebuilt {
    /// The commit the change's own commits now end at, which is the base itself
    /// where the branch has none of its own.
    pub tip: Oid,
    /// The tree of [`Self::tip`], so the caller can tell whether the change's
    /// current tree still has to be committed on top.
    pub tip_tree: Oid,
    /// Whether the commits the branch carried were rewritten, and so whether
    /// the push has to be forced.
    ///
    /// False for a branch that had no commits of its own to rewrite — one whose
    /// pull request this run is opening, which has nothing on the remote to
    /// force past.
    pub rewritten: bool,
    /// What became of those commits, for the caller to report.
    pub outcome: Outcome,
}

/// What a rebuild did to the commits the branch was carrying.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The branch already sat on the base it belongs on, so its commits were
    /// left exactly where they were and the push is an ordinary one.
    Kept,
    /// The commits were replayed onto the new base, and this many of them are
    /// on the branch now. Fewer than were there before means a replay came out
    /// empty — the base had caught up with that round of the change — which is
    /// what GitHub's own rebase does with such a commit too.
    Replayed(usize),
    /// The commits could not be brought along, so the branch starts again from
    /// the base with nothing on it.
    Collapsed(Collapse),
}

/// Why a rebuild had to start the branch again.
#[derive(Debug, PartialEq, Eq)]
pub enum Collapse {
    /// The branch is not a chain of single-parent commits on its base: it was
    /// pushed under one of the merging strategies, or somebody pushed to it by
    /// hand. There is no sequence of the change's own commits to replay.
    Shape,
    /// Replaying one of the commits conflicted with the new base. Only the
    /// intermediate states of the change can do this — jj has already rebased
    /// the change itself, so the final tree is known to be conflict-free — and
    /// the branch ends up carrying that final tree in one commit.
    Conflict,
}

impl Rebuilt {
    /// What to tell the user about the rebuild, or `None` where there is
    /// nothing worth saying — a branch whose commits stayed put is the ordinary
    /// case, and a branch that had none is a pull request being opened.
    pub fn describe(&self, branch_name: &str) -> Option<(&'static str, String)> {
        match self.outcome {
            Outcome::Kept => None,
            // Nothing was on the branch to replay, so nothing happened to it:
            // this is a pull request being opened. Told apart from a replay that
            // came out empty by `rewritten`, which is the same question — was
            // there anything there — asked for the push.
            Outcome::Replayed(0) if !self.rewritten => None,
            Outcome::Replayed(0) => Some((
                "♻️",
                format!(
                    "Replaying the commits {branch_name} carried onto the new base left nothing \
                     of them: the base already has what they did. Force-pushing the branch back \
                     to it"
                ),
            )),
            Outcome::Replayed(commits) => Some((
                "♻️",
                format!(
                    "Replayed {commits} commit{} onto the new base, force-pushing {branch_name}",
                    if commits == 1 { "" } else { "s" }
                ),
            )),
            Outcome::Collapsed(Collapse::Shape) => Some((
                "♻️",
                format!(
                    "Rebuilt {branch_name} as a single commit: what it carried was not a chain \
                     of commits on its base, which is what a pull request pushed under another \
                     spr.baseStrategy looks like"
                ),
            )),
            Outcome::Collapsed(Collapse::Conflict) => Some((
                "⚠️",
                format!(
                    "Rebuilt {branch_name} as a single commit: replaying the commits it carried \
                     onto the new base conflicted, so the review history could not be kept"
                ),
            )),
        }
    }
}

/// Put the change's own commits on `onto`, replaying them where they are not
/// there already.
///
/// `head` is where the pull request branch points now and `base` is the commit
/// those own commits sit on — the merge base of the branch and what the pull
/// request is based on, which is the only thing that says where the change's
/// own history starts. `onto` is where they belong: the head commit of the pull
/// request below, the master commit the change is based on, or a base commit
/// this run built.
///
/// A branch already sitting on `onto` is left alone, which is what makes an
/// ordinary amend an ordinary push: only a base that moved costs a rewrite.
///
/// Never fails for anything about the branch itself. A shape this cannot work
/// with and a replay that conflicts both come back as
/// [`Outcome::Collapsed`] — with the branch starting again from `onto` — because
/// the alternative is refusing to push a change whose local state is perfectly
/// good, and the commit the caller puts on top carries the change either way.
/// What is lost is the review history, which is worth a warning and not a
/// failure.
pub fn rebuild(repo: &git2::Repository, head: Oid, base: Oid, onto: Oid) -> Result<Rebuilt> {
    let own = own_commits(repo, head, base)?;

    // Nothing to do to a branch that is already where it belongs — but only if
    // what it carries is a chain: a branch of another shape is rebuilt even
    // though its base has not moved, or it would stay unrebasable for as long
    // as nothing else about the change did.
    if onto == base && own.is_some() {
        return Ok(Rebuilt {
            tip: head,
            tip_tree: tree_of(repo, head)?,
            rewritten: false,
            outcome: Outcome::Kept,
        });
    }

    let replayed = match &own {
        Some(own) => replay(repo, own, onto)?,
        None => None,
    };

    let (tip, outcome) = match replayed {
        Some(replayed) => (
            replayed.last().copied().unwrap_or(onto),
            Outcome::Replayed(replayed.len()),
        ),
        None => (
            onto,
            Outcome::Collapsed(match own {
                Some(_) => Collapse::Conflict,
                None => Collapse::Shape,
            }),
        ),
    };

    Ok(Rebuilt {
        tip,
        tip_tree: tree_of(repo, tip)?,
        // A branch with no commits of its own is not being rewritten by being
        // given some: there is nothing on the remote that this push has to get
        // past. Everything else here rewrites, including a collapse, which is
        // the most thorough rewrite of all.
        rewritten: !own.as_ref().is_some_and(Vec::is_empty),
        outcome,
    })
}

/// Whether `head` carries the change's own commits as a chain on `base` — the
/// shape [`rebuild`] can keep, and the shape GitHub can rebase.
///
/// Asked by a run that would otherwise leave the branch alone because its trees
/// are already right: a branch of any other shape has to be rebuilt anyway, or
/// it would stay unrebasable for as long as nothing else changes.
pub fn is_a_chain(repo: &git2::Repository, head: Oid, base: Oid) -> Result<bool> {
    Ok(own_commits(repo, head, base)?.is_some())
}

/// The commits `head` carries on top of `base`, oldest first, or `None` where
/// what it carries is not a chain of the change's own commits.
///
/// The chain has to be single-parent all the way down to `base`. A merge commit
/// in it means the branch was pushed under a merging strategy — or by somebody
/// else — and there is no telling which of its ancestors are the change's own
/// rounds and which came in from the side, so the caller is told nothing rather
/// than a guess it would replay.
///
/// `Some(vec![])` is the branch that is already exactly at its base: a pull
/// request being opened, which has no commits to keep and needs none.
fn own_commits(repo: &git2::Repository, head: Oid, base: Oid) -> Result<Option<Vec<Oid>>> {
    // Containment first, so that a branch which does not descend from the base
    // at all — one GitHub rebased onto something else, say — is turned down
    // before the walk rather than by running out of patience.
    if head != base && !repo.graph_descendant_of(head, base)? {
        return Ok(None);
    }

    let mut commits = Vec::new();
    let mut oid = head;

    while oid != base {
        if commits.len() >= WALK_LIMIT {
            return Ok(None);
        }

        let commit = repo.find_commit(oid)?;
        if commit.parent_count() != 1 {
            return Ok(None);
        }

        commits.push(oid);
        oid = commit.parent_id(0)?;
    }

    commits.reverse();

    Ok(Some(commits))
}

/// Replay `commits` onto `onto`, keeping each one's message, author and
/// timestamps, or `None` where one of them conflicts.
///
/// A commit whose replay changes nothing — because `onto` already has what it
/// did — is dropped rather than pushed as an empty commit, which is also what
/// GitHub's rebase does with one.
///
/// Keeping the signatures rather than stamping the replay with the time it
/// happened is what makes a replay repeatable: the same commits onto the same
/// base come out with the same ids, so a run that finds nothing to do pushes
/// exactly what is already on the remote instead of a fresh set of commits with
/// the same content.
fn replay(repo: &git2::Repository, commits: &[Oid], onto: Oid) -> Result<Option<Vec<Oid>>> {
    let mut replayed = Vec::with_capacity(commits.len());
    let mut tip = onto;

    for &oid in commits {
        let commit = repo.find_commit(oid)?;
        let tip_commit = repo.find_commit(tip)?;

        let mut index = repo.cherrypick_commit(&commit, &tip_commit, 0, None)?;
        if index.has_conflicts() {
            return Ok(None);
        }

        let tree = index.write_tree_to(repo)?;
        if tree == tip_commit.tree_id() {
            continue;
        }

        tip = repo.commit(
            None,
            &commit.author(),
            &commit.committer(),
            // Lossy only for a commit message that is not UTF-8, which nothing
            // jj-spr pushes ever is; the alternative is refusing to replay it.
            &String::from_utf8_lossy(commit.message_bytes()),
            &repo.find_tree(tree)?,
            &[&tip_commit],
        )?;
        replayed.push(tip);
    }

    Ok(Some(replayed))
}

fn tree_of(repo: &git2::Repository, oid: Oid) -> Result<Oid> {
    Ok(repo
        .find_commit(oid)
        .map_err(|error| Error::new(format!("could not read commit {oid}: {error}")))?
        .tree_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    /// A repository to build branch shapes in, with a helper for each shape
    /// this module has an opinion about.
    struct Fixture {
        _dir: TempDir,
        repo: git2::Repository,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().expect("temp dir");
            let repo = git2::Repository::init(dir.path()).expect("git init");

            Self { _dir: dir, repo }
        }

        fn signature(&self) -> git2::Signature<'static> {
            git2::Signature::new(
                "Test User",
                "test@example.com",
                &git2::Time::new(1_700_000_000, 0),
            )
            .expect("signature")
        }

        /// A commit whose tree holds `files`, on `parents`.
        fn commit(&self, message: &str, files: &[(&str, &str)], parents: &[Oid]) -> Oid {
            let mut builder = self.repo.treebuilder(None).expect("treebuilder");
            for (name, content) in files {
                let blob = self.repo.blob(content.as_bytes()).expect("blob");
                builder.insert(name, blob, 0o100644).expect("insert");
            }
            let tree = builder.write().expect("write tree");

            let parents: Vec<_> = parents
                .iter()
                .map(|oid| self.repo.find_commit(*oid).expect("parent"))
                .collect();
            let parent_refs: Vec<_> = parents.iter().collect();

            let signature = self.signature();
            self.repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    message,
                    &self.repo.find_tree(tree).expect("tree"),
                    &parent_refs,
                )
                .expect("commit")
        }

        fn tree_of(&self, oid: Oid) -> Oid {
            self.repo.find_commit(oid).expect("commit").tree_id()
        }

        /// The messages of the commits from `head` down to `base`, oldest
        /// first, which is how a replay's result is checked without depending
        /// on ids.
        fn messages(&self, head: Oid, base: Oid) -> Vec<String> {
            own_commits(&self.repo, head, base)
                .expect("walk")
                .expect("a chain")
                .into_iter()
                .map(|oid| {
                    self.repo
                        .find_commit(oid)
                        .expect("commit")
                        .summary()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        }
    }

    /// The shape a pull request branch has under `linear-rebase`: the base, and
    /// the change's own rounds on top of it.
    fn chain(fixture: &Fixture) -> (Oid, Oid) {
        let base = fixture.commit("base", &[("base.txt", "base\n")], &[]);
        let first = fixture.commit(
            "round one",
            &[("base.txt", "base\n"), ("a.txt", "1\n")],
            &[base],
        );
        let second = fixture.commit(
            "round two",
            &[("base.txt", "base\n"), ("a.txt", "2\n")],
            &[first],
        );

        (base, second)
    }

    #[test]
    fn own_commits_are_the_chain_above_the_base() {
        let fixture = Fixture::new();
        let (base, head) = chain(&fixture);

        assert_eq!(
            fixture.messages(head, base),
            vec!["round one".to_string(), "round two".to_string()],
            "oldest first, so a replay can walk them in order"
        );
    }

    /// A branch that is exactly at its base has no commits of its own, which is
    /// the pull request that has not been opened yet.
    #[test]
    fn a_branch_at_its_base_carries_nothing() {
        let fixture = Fixture::new();
        let base = fixture.commit("base", &[("base.txt", "base\n")], &[]);

        assert_eq!(
            own_commits(&fixture.repo, base, base).expect("walk"),
            Some(Vec::new())
        );
    }

    /// The shape the merging strategies push. There is no telling which of a
    /// merge's ancestors are the change's own rounds, so the branch is refused.
    #[test]
    fn a_merge_commit_is_not_a_chain() {
        let fixture = Fixture::new();
        let base = fixture.commit("base", &[("base.txt", "base\n")], &[]);
        let side = fixture.commit("side", &[("base.txt", "base\n"), ("s.txt", "s\n")], &[base]);
        let merged = fixture.commit(
            "merged",
            &[("base.txt", "base\n"), ("s.txt", "s\n"), ("a.txt", "1\n")],
            &[base, side],
        );

        assert_eq!(
            own_commits(&fixture.repo, merged, base).expect("walk"),
            None
        );
    }

    /// A branch that does not descend from the base at all — the shape GitHub
    /// leaves behind when it rebases a branch onto something else — is turned
    /// down rather than walked to the root.
    #[test]
    fn a_branch_that_does_not_descend_from_the_base_is_not_a_chain() {
        let fixture = Fixture::new();
        let base = fixture.commit("base", &[("base.txt", "base\n")], &[]);
        let elsewhere = fixture.commit("elsewhere", &[("e.txt", "e\n")], &[]);

        assert_eq!(
            own_commits(&fixture.repo, elsewhere, base).expect("walk"),
            None
        );
    }

    /// The ordinary amend: the base has not moved, so the commits stay exactly
    /// where they are and the push does not have to force.
    #[test]
    fn a_branch_on_its_base_is_kept() {
        let fixture = Fixture::new();
        let (base, head) = chain(&fixture);

        let rebuilt = rebuild(&fixture.repo, head, base, base).expect("rebuild");

        assert_eq!(rebuilt.outcome, Outcome::Kept);
        assert_eq!(rebuilt.tip, head);
        assert_eq!(rebuilt.tip_tree, fixture.tree_of(head));
        assert!(!rebuilt.rewritten, "keeping the commits forces nothing");
    }

    /// The point of the strategy: the base moved, and the change's own commits
    /// come along one by one, keeping their messages and their number.
    #[test]
    fn a_moved_base_replays_the_commits() {
        let fixture = Fixture::new();
        let (base, head) = chain(&fixture);
        // What the change below pushed: the base, plus an edit of its own to a
        // file this change does not touch.
        let moved = fixture.commit(
            "the change below, amended",
            &[("base.txt", "base\n"), ("below.txt", "below\n")],
            &[base],
        );

        let rebuilt = rebuild(&fixture.repo, head, base, moved).expect("rebuild");

        assert_eq!(rebuilt.outcome, Outcome::Replayed(2));
        assert!(rebuilt.rewritten, "the commits moved, so the push forces");
        assert_eq!(
            fixture.messages(rebuilt.tip, moved),
            vec!["round one".to_string(), "round two".to_string()],
            "the review history comes along, rebased"
        );

        let tree = fixture
            .repo
            .find_commit(rebuilt.tip)
            .expect("tip")
            .tree()
            .expect("tree");
        assert!(
            tree.get_name("below.txt").is_some(),
            "the replayed branch carries what the base moved to"
        );
        assert!(
            tree.get_name("a.txt").is_some(),
            "...and the change's own work"
        );
    }

    /// A round of the change that the new base has already taken care of would
    /// replay as an empty commit. GitHub's own rebase drops those, and so does
    /// this: what matters is that the branch ends up with the change's tree.
    #[test]
    fn a_replay_that_changes_nothing_is_dropped() {
        let fixture = Fixture::new();
        let base = fixture.commit("base", &[("a.txt", "1\n")], &[]);
        let head = fixture.commit("round one", &[("a.txt", "2\n")], &[base]);
        // The base moved to exactly what that round did.
        let moved = fixture.commit("the change below, amended", &[("a.txt", "2\n")], &[base]);

        let rebuilt = rebuild(&fixture.repo, head, base, moved).expect("rebuild");

        assert_eq!(rebuilt.outcome, Outcome::Replayed(0));
        assert_eq!(rebuilt.tip, moved, "the branch is left at its base");
        assert!(
            rebuilt.rewritten,
            "the commit that was there is gone, which no ordinary push can do"
        );
        assert!(
            rebuilt.describe("spr/a-branch").is_some(),
            "a branch whose commits all went away is worth telling the user about"
        );
    }

    /// Replaying the same commits onto the same base twice has to come out the
    /// same, or every run would push a fresh set of commits with the same
    /// content and every reviewer would see a force-push that changed nothing.
    #[test]
    fn replaying_twice_gives_the_same_commits() {
        let fixture = Fixture::new();
        let (base, head) = chain(&fixture);
        let moved = fixture.commit(
            "the change below, amended",
            &[("base.txt", "base\n"), ("below.txt", "below\n")],
            &[base],
        );

        let first = rebuild(&fixture.repo, head, base, moved).expect("rebuild");
        let again = rebuild(&fixture.repo, first.tip, moved, moved).expect("rebuild");

        assert_eq!(again.outcome, Outcome::Kept);
        assert_eq!(again.tip, first.tip);

        // ...and replaying the original chain again, rather than the replayed
        // one, lands on the same commits.
        let repeated = rebuild(&fixture.repo, head, base, moved).expect("rebuild");
        assert_eq!(repeated.tip, first.tip);
    }

    /// A branch pushed under one of the merging strategies has no chain to
    /// replay, so it starts again from the base — which is how a pull request
    /// migrates onto this strategy.
    #[test]
    fn a_merge_shaped_branch_collapses() {
        let fixture = Fixture::new();
        let base = fixture.commit("base", &[("base.txt", "base\n")], &[]);
        let side = fixture.commit("side", &[("base.txt", "base\n"), ("s.txt", "s\n")], &[base]);
        let merged = fixture.commit(
            "merged",
            &[("base.txt", "base\n"), ("s.txt", "s\n"), ("a.txt", "1\n")],
            &[base, side],
        );

        let rebuilt = rebuild(&fixture.repo, merged, base, base).expect("rebuild");

        assert_eq!(rebuilt.outcome, Outcome::Collapsed(Collapse::Shape));
        assert_eq!(rebuilt.tip, base, "the branch starts again from its base");
        assert!(rebuilt.rewritten);
        assert!(
            rebuilt.describe("spr/a-branch").is_some(),
            "a collapse is worth telling the user about"
        );
    }

    /// An intermediate round of the change can conflict with the new base even
    /// though the change itself does not — jj has already rebased that. The
    /// branch keeps the change rather than the history.
    #[test]
    fn a_conflicting_replay_collapses() {
        let fixture = Fixture::new();
        let base = fixture.commit("base", &[("a.txt", "base\n")], &[]);
        let head = fixture.commit("round one", &[("a.txt", "mine\n")], &[base]);
        // The base moved that same line somewhere else, so replaying the round
        // has no common ground to merge on.
        let moved = fixture.commit("the change below", &[("a.txt", "theirs\n")], &[base]);

        let rebuilt = rebuild(&fixture.repo, head, base, moved).expect("rebuild");

        assert_eq!(rebuilt.outcome, Outcome::Collapsed(Collapse::Conflict));
        assert_eq!(rebuilt.tip, moved);
        assert!(rebuilt.rewritten);
    }

    /// A pull request being opened has a branch that does not exist yet: there
    /// is nothing to replay and nothing to force past, however far the base it
    /// is being given is from the master commit the walk starts at.
    #[test]
    fn an_unopened_pull_request_forces_nothing() {
        let fixture = Fixture::new();
        let master = fixture.commit("master", &[("base.txt", "base\n")], &[]);
        let below = fixture.commit(
            "the change below",
            &[("base.txt", "base\n"), ("below.txt", "below\n")],
            &[master],
        );

        let rebuilt = rebuild(&fixture.repo, master, master, below).expect("rebuild");

        assert_eq!(rebuilt.outcome, Outcome::Replayed(0));
        assert_eq!(rebuilt.tip, below);
        assert!(
            !rebuilt.rewritten,
            "there is no branch on the remote to force past"
        );
        assert!(rebuilt.describe("spr/a-branch").is_none());
    }

    /// Not a test of this module so much as of the assumption the walk rests
    /// on: `git2` reports a root commit as having no parents rather than
    /// erroring, so the walk turns such a branch down instead of failing.
    #[test]
    fn a_root_commit_ends_the_walk() {
        let fixture = Fixture::new();
        let root = fixture.commit("root", &[("a.txt", "1\n")], &[]);
        let head = fixture.commit("round one", &[("a.txt", "2\n")], &[root]);
        let unrelated = fixture.commit("unrelated", &[("b.txt", "b\n")], &[]);

        assert_eq!(
            own_commits(&fixture.repo, head, unrelated).expect("walk"),
            None
        );
        assert!(Path::new(fixture.repo.path()).exists());
    }
}
