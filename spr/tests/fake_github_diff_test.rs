/*
 * What `jj spr diff` decides for itself, tested without a network.
 *
 * Two stand-ins make that possible, and they are stand-ins of different kinds:
 *
 * - **The remote is a real bare repository.** Every push goes to it, so every
 *   assertion about a branch — its shape, its commits, whether it was written
 *   past or rewritten — is about what git actually did. Nothing here fakes that,
 *   and the force-with-lease pushes are honoured for real.
 * - **GitHub is a fake**, holding pull requests and stacks in memory. It answers
 *   from its own state and from the bare repository, and records what it was
 *   asked so that a test can assert the calls as well as their result.
 *
 * The division of labour with `github_e2e_test.rs` is the thing to keep straight,
 * and it is not "fast tests here, slow tests there". A test against a fake can
 * only assert what jj-spr does; what GitHub does about it is what the end-to-end
 * suite is for, and that suite is also where the fake's rules are checked. Every
 * rule the fake reproduces is marked below with the live test that pins it — if
 * one of those rules is ever wrong, the fake will be confidently wrong with it,
 * and the live test is what says so.
 */

use std::{cell::RefCell, path::Path, path::PathBuf, process::Command};

use jj_spr::{
    commands::diff::{DiffOptions, diff},
    config::{BaseStrategy, Config, StackDisplay},
    error::{Error, Result},
    github::{
        GitHubApi, GitHubBranch, PullRequest, PullRequestRequestReviewers, PullRequestState,
        PullRequestUpdate, Stack, StackApiError, StackBase, StackGitRef, StackPullRequest,
        StackPullRequestState, StackResult, UnstackOutcome, base_branch_to_take_away,
    },
    jj::Jujutsu,
    message::MessageSectionsMap,
};

const BRANCH_PREFIX: &str = "spr/test/";
const MASTER: &str = "main";

// ---------------------------------------------------------------------------
// The fake GitHub

/// One pull request, as the fake holds it.
#[derive(Debug, Clone)]
struct FakePullRequest {
    number: u64,
    base: String,
    head: String,
    title: String,
    sections: MessageSectionsMap,
    state: PullRequestState,
    draft: bool,
}

/// One stack, as the fake holds it. Members are bottom first, and a merged one
/// stays in the list for ever — see [`FakeGitHub::unstack`].
#[derive(Debug, Clone)]
struct FakeStack {
    number: u64,
    open: bool,
    members: Vec<u64>,
}

/// A call the fake was asked to make, in the order it was asked.
///
/// The point of recording these is the questions a result cannot answer: whether
/// a base was moved at all, and whether the stack was taken apart *before* it —
/// which is the order GitHub requires and the ordinary way to get it wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Created {
        number: u64,
        base: String,
        head: String,
    },
    Updated {
        number: u64,
        base: Option<String>,
    },
    Retargeted {
        number: u64,
        to: String,
    },
    StackCreated {
        members: Vec<u64>,
    },
    AddedToStack {
        stack: u64,
        members: Vec<u64>,
    },
    Unstacked {
        stack: u64,
    },
}

#[derive(Debug, Default)]
struct FakeState {
    pull_requests: Vec<FakePullRequest>,
    stacks: Vec<FakeStack>,
    calls: Vec<Call>,
    next_number: u64,
    next_stack_number: u64,
}

/// A GitHub that keeps its pull requests and stacks in memory and its branches in
/// a bare repository.
struct FakeGitHub {
    config: Config,
    /// The bare repository the pushes go to, which is where the fake reads every
    /// commit it reports.
    remote: PathBuf,
    state: RefCell<FakeState>,
}

impl FakeGitHub {
    fn new(config: Config, remote: PathBuf) -> Self {
        Self {
            config,
            remote,
            state: RefCell::new(FakeState {
                next_number: 1,
                next_stack_number: 100,
                ..Default::default()
            }),
        }
    }

    fn branch(&self, name: &str) -> GitHubBranch {
        GitHubBranch::new_from_branch_name(name, "origin", MASTER)
    }

    /// What `branch` points at on the remote, or the zero oid where the remote
    /// does not have it — which is what the real client reports when it cannot
    /// resolve the ref either.
    fn tip(&self, branch: &str) -> git2::Oid {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");

        repo.find_reference(&format!("refs/heads/{branch}"))
            .ok()
            .and_then(|reference| reference.target())
            .unwrap_or_else(git2::Oid::zero)
    }

    fn calls(&self) -> Vec<Call> {
        self.state.borrow().calls.clone()
    }

    /// The base branch of a pull request, by number.
    fn base_of(&self, number: u64) -> String {
        self.state
            .borrow()
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == number)
            .unwrap_or_else(|| panic!("the fake should hold PR #{number}"))
            .base
            .clone()
    }

    fn head_of(&self, number: u64) -> String {
        self.state
            .borrow()
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == number)
            .unwrap_or_else(|| panic!("the fake should hold PR #{number}"))
            .head
            .clone()
    }

    fn open_stacks(&self) -> Vec<FakeStack> {
        self.state
            .borrow()
            .stacks
            .iter()
            .filter(|stack| stack.open)
            .cloned()
            .collect()
    }

    /// The open stack holding `number`, as GitHub's shape.
    fn stack_holding(&self, number: u64) -> Option<Stack> {
        let state = self.state.borrow();
        let stack = state
            .stacks
            .iter()
            .find(|stack| stack.open && stack.members.contains(&number))?;

        Some(self.as_stack(&state, stack))
    }

    fn as_stack(&self, state: &FakeState, stack: &FakeStack) -> Stack {
        Stack {
            id: stack.number,
            number: stack.number,
            base: StackBase {
                ref_name: MASTER.to_string(),
            },
            open: stack.open,
            pull_requests: stack
                .members
                .iter()
                .filter_map(|number| {
                    let pull_request = state
                        .pull_requests
                        .iter()
                        .find(|pull_request| pull_request.number == *number)?;

                    Some(StackPullRequest {
                        number: *number,
                        state: match pull_request.state {
                            PullRequestState::Open => StackPullRequestState::Open,
                            PullRequestState::Closed => StackPullRequestState::Closed,
                        },
                        head: StackGitRef {
                            ref_name: pull_request.head.clone(),
                            sha: Some(format!("{}", self.tip(&pull_request.head))),
                        },
                        base: Some(StackGitRef {
                            ref_name: pull_request.base.clone(),
                            sha: None,
                        }),
                        draft: pull_request.draft,
                        merged_at: None,
                        title: Some(pull_request.title.clone()),
                    })
                })
                .collect(),
            created_at: None,
        }
    }

    /// GitHub's rule: a stack owns its members' base refs, so any update
    /// carrying a base is refused while a stack holds the pull request — whether
    /// or not the value differs.
    ///
    /// Pinned live by `retargeting_a_pull_request_in_a_github_stack_takes_it_out_of_the_stack`,
    /// which is the test that would fail if GitHub ever allowed it. jj-spr's
    /// whole `unlock_base` dance exists for this rule, so a fake that let a base
    /// move while stacked would test a jj-spr that does not need to exist.
    fn refuse_a_stacked_base_change(&self, number: u64) -> Result<()> {
        if self
            .state
            .borrow()
            .stacks
            .iter()
            .any(|stack| stack.open && stack.members.contains(&number))
        {
            return Err(Error::new(format!(
                "fake GitHub: Pull Request #{number} is in a stack, so its base ref cannot be \
                 changed (this is GitHub's 403)"
            )));
        }

        Ok(())
    }

    /// Set the base of a pull request, subject to the rule above.
    fn set_base(&self, number: u64, base: &str) -> Result<()> {
        self.refuse_a_stacked_base_change(number)?;

        let mut state = self.state.borrow_mut();
        let pull_request = state
            .pull_requests
            .iter_mut()
            .find(|pull_request| pull_request.number == number)
            .ok_or_else(|| Error::new(format!("fake GitHub: no Pull Request #{number}")))?;
        pull_request.base = base.to_string();

        Ok(())
    }

    /// Delete a branch from the remote, reporting whether it was there.
    fn delete_branch(&self, branch: &GitHubBranch) -> bool {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");

        match repo.find_reference(&format!("refs/heads/{}", branch.branch_name())) {
            Ok(mut reference) => {
                reference.delete().expect("deleting a branch");
                true
            }
            Err(_) => false,
        }
    }

    /// Whether the pull requests chain base-to-head, bottom first, which is what
    /// GitHub's stacks require of their members.
    ///
    /// Pinned live by `a_stack_pushed_by_jj_spr_becomes_a_github_stack`: that a
    /// chain jj-spr pushes satisfies GitHub is the whole of what it asserts.
    fn is_a_chain(&self, state: &FakeState, pull_requests: &[u64]) -> bool {
        let of = |number: &u64| {
            state
                .pull_requests
                .iter()
                .find(|pull_request| pull_request.number == *number)
        };

        pull_requests
            .windows(2)
            .all(|pair| match (of(&pair[0]), of(&pair[1])) {
                (Some(below), Some(above)) => above.base == below.head,
                _ => false,
            })
    }
}

impl GitHubApi for FakeGitHub {
    async fn get_pull_request(&self, number: u64) -> Result<PullRequest> {
        let pull_request = self
            .state
            .borrow()
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == number)
            .cloned()
            .ok_or_else(|| Error::new(format!("fake GitHub: no Pull Request #{number}")))?;

        Ok(PullRequest {
            number,
            node_id: format!("PR_fake{number}"),
            state: pull_request.state.clone(),
            title: pull_request.title.clone(),
            body: Some(jj_spr::message::build_github_body(&pull_request.sections)),
            sections: pull_request.sections.clone(),
            base: self.branch(&pull_request.base),
            head: self.branch(&pull_request.head),
            // Read off the remote at the moment of the call, exactly as the real
            // client does: it fetches the two refs and reads them back. A test
            // that pushes between two calls sees the difference, which is what
            // makes the "read every pull request before pushing anything" rule
            // testable at all.
            base_oid: self.tip(&pull_request.base),
            head_oid: self.tip(&pull_request.head),
            merge_commit: None,
            reviewers: Default::default(),
            review_status: None,
        })
    }

    async fn create_pull_request(
        &self,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64> {
        let mut state = self.state.borrow_mut();
        let number = state.next_number;
        state.next_number += 1;

        state.pull_requests.push(FakePullRequest {
            number,
            base: base_ref_name.clone(),
            head: head_ref_name.clone(),
            title: message
                .get(&jj_spr::message::MessageSection::Title)
                .cloned()
                .unwrap_or_default(),
            sections: message.clone(),
            state: PullRequestState::Open,
            draft,
        });
        state.calls.push(Call::Created {
            number,
            base: base_ref_name,
            head: head_ref_name,
        });

        Ok(number)
    }

    async fn update_pull_request(&self, number: u64, updates: PullRequestUpdate) -> Result<()> {
        if let Some(base) = &updates.base {
            self.set_base(number, base)?;
        }

        {
            let mut state = self.state.borrow_mut();
            if let Some(pull_request) = state
                .pull_requests
                .iter_mut()
                .find(|pull_request| pull_request.number == number)
            {
                if let Some(title) = updates.title.clone() {
                    pull_request.title = title;
                }
                if let Some(state) = updates.state.clone() {
                    pull_request.state = state;
                }
            }

            state.calls.push(Call::Updated {
                number,
                base: updates.base.clone(),
            });
        }

        Ok(())
    }

    async fn retarget_pull_request(
        &self,
        number: u64,
        new_base: &GitHubBranch,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        self.set_base(number, new_base.branch_name())?;
        self.state.borrow_mut().calls.push(Call::Retargeted {
            number,
            to: new_base.branch_name().to_string(),
        });

        // The same rule the real client applies, from the same function, so that
        // a test asserting a branch went away is asserting jj-spr's rule rather
        // than the fake's reading of it.
        Ok(
            match base_branch_to_take_away(&self.config, new_base, old_base) {
                Some(old_base) => self.delete_branch(old_base),
                None => false,
            },
        )
    }

    async fn retarget_to_master_branch(
        &self,
        number: u64,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        let master = self.config.master_ref.clone();

        self.retarget_pull_request(number, &master, old_base).await
    }

    async fn request_reviewers(
        &self,
        _number: u64,
        _reviewers: PullRequestRequestReviewers,
    ) -> Result<()> {
        Ok(())
    }

    async fn get_open_stack_for_pull_request(&self, number: u64) -> StackResult<Option<Stack>> {
        Ok(self.stack_holding(number))
    }

    async fn create_stack(&self, pull_requests: &[u64]) -> StackResult<Stack> {
        if pull_requests.len() < 2 {
            return Err(StackApiError::TooFewPullRequests);
        }

        let mut state = self.state.borrow_mut();

        let already: Vec<u64> = pull_requests
            .iter()
            .copied()
            .filter(|number| {
                state
                    .stacks
                    .iter()
                    .any(|stack| stack.open && stack.members.contains(number))
            })
            .collect();
        if !already.is_empty() {
            return Err(StackApiError::AlreadyStacked {
                pull_requests: already,
            });
        }

        if !self.is_a_chain(&state, pull_requests) {
            return Err(StackApiError::NotAChain);
        }

        let number = state.next_stack_number;
        state.next_stack_number += 1;
        let stack = FakeStack {
            number,
            open: true,
            members: pull_requests.to_vec(),
        };
        state.stacks.push(stack.clone());
        state.calls.push(Call::StackCreated {
            members: pull_requests.to_vec(),
        });

        Ok(self.as_stack(&state, &stack))
    }

    async fn add_to_stack(&self, stack_number: u64, pull_requests: &[u64]) -> StackResult<Stack> {
        let mut state = self.state.borrow_mut();

        let Some(position) = state
            .stacks
            .iter()
            .position(|stack| stack.number == stack_number && stack.open)
        else {
            return Err(StackApiError::StackNotFound { stack_number });
        };

        // Appending has to keep the chain: the first of the new members is based
        // on the branch of the stack's current top.
        let mut chain = state.stacks[position].members.clone();
        chain.extend_from_slice(pull_requests);
        if !self.is_a_chain(&state, &chain) {
            return Err(StackApiError::NotAChain);
        }

        state.stacks[position].members = chain;
        let stack = state.stacks[position].clone();
        state.calls.push(Call::AddedToStack {
            stack: stack_number,
            members: pull_requests.to_vec(),
        });

        Ok(self.as_stack(&state, &stack))
    }

    async fn unstack(&self, stack_number: u64) -> StackResult<UnstackOutcome> {
        let mut state = self.state.borrow_mut();

        let Some(position) = state
            .stacks
            .iter()
            .position(|stack| stack.number == stack_number)
        else {
            return Err(StackApiError::StackNotFound { stack_number });
        };

        state.calls.push(Call::Unstacked {
            stack: stack_number,
        });

        // Nothing here has merged anything, so every member is released and the
        // stack record goes. The other outcome — a stack retained because it
        // holds a merged member — belongs to `land`, and to the live suite.
        state.stacks.remove(position);

        Ok(UnstackOutcome::Dissolved)
    }
}

// ---------------------------------------------------------------------------
// The repository harness

/// A jj repository with a bare repository standing in for GitHub's remote.
struct Local {
    _dir: tempfile::TempDir,
    remote: PathBuf,
    repo: PathBuf,
}

fn run(program: &str, args: &[&str], dir: &Path) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Local {
    /// A repository with one commit on the remote's `main`, and nothing else.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let remote = dir.path().join("remote.git");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("the repository directory");

        run(
            "git",
            &[
                "init",
                "--bare",
                "--initial-branch",
                MASTER,
                remote.to_str().unwrap(),
            ],
            dir.path(),
        );

        run("jj", &["git", "init", "--colocate"], &repo);
        for (key, value) in [
            ("user.name", "jj-spr test"),
            ("user.email", "test@example.com"),
        ] {
            run("jj", &["config", "set", "--repo", key, value], &repo);
        }
        run(
            "jj",
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
            &repo,
        );

        // Seed the remote's default branch, which is what every change here is
        // measured against.
        std::fs::write(repo.join("seed.txt"), "seed\n").expect("the seed file");
        run("jj", &["commit", "-m", "seed"], &repo);
        run("jj", &["bookmark", "create", MASTER, "-r", "@-"], &repo);
        run("jj", &["git", "push", "-b", MASTER], &repo);

        Self {
            _dir: dir,
            remote,
            repo,
        }
    }

    fn config(&self, base_strategy: BaseStrategy, stack_display: StackDisplay) -> Config {
        Config {
            base_strategy,
            stack_display,
            ..Config::new(
                "acme".into(),
                "widgets".into(),
                "origin".into(),
                MASTER.into(),
                BRANCH_PREFIX.into(),
                false,
            )
        }
    }

    fn github(&self, config: &Config) -> FakeGitHub {
        FakeGitHub::new(config.clone(), self.remote.clone())
    }

    fn jj(&self) -> Jujutsu {
        Jujutsu::new(self.repo.clone()).expect("a Jujutsu repository")
    }

    /// Stack one change per title on the default branch, each adding a file of
    /// its own, leaving the working copy on the top one.
    fn stack(&self, titles: &[&str]) {
        // The first change goes on the trunk rather than on the working copy,
        // which after seeding is an empty commit on it: a run given a range that
        // reached that commit would be asked to push a change with no message.
        for (position, title) in titles.iter().enumerate() {
            let mut args = vec!["new"];
            if position == 0 {
                args.push("trunk()");
            }
            let message = describe(title);
            args.extend_from_slice(&["-m", &message]);

            run("jj", &args, &self.repo);
            self.write(title, title);
        }
    }

    /// Give the change `title` new content, which is what an amend is here.
    fn write(&self, title: &str, content: &str) {
        std::fs::write(self.repo.join(slug(title)), format!("{content}\n")).expect("a test file");
    }

    /// Run `jj spr diff` over the whole local stack.
    ///
    /// No `-m`, which is what opening a pull request looks like: the first commit
    /// on a branch takes the local change's description, and a message given here
    /// would stand in for it.
    async fn diff(&self, gh: &FakeGitHub, config: &Config) -> Result<()> {
        self.diff_with(gh, config, &["--all", "-r", "trunk()..@"])
            .await
    }

    /// Run `jj spr diff` over the whole local stack, wording the commits it adds
    /// to the branches — which is what an update to an existing pull request
    /// needs.
    async fn amend(&self, gh: &FakeGitHub, config: &Config, message: &str) -> Result<()> {
        self.diff_with(gh, config, &["--all", "-r", "trunk()..@", "-m", message])
            .await
    }

    async fn diff_with(&self, gh: &FakeGitHub, config: &Config, args: &[&str]) -> Result<()> {
        let opts = diff_options(args);

        diff(opts, &self.jj(), gh, config).await
    }

    /// The commits `head` carries on top of `base` on the remote, oldest first,
    /// each as `<number of parents> <first line>`.
    ///
    /// The same shape the end-to-end suite reads off GitHub's compare endpoint,
    /// and for the same reason: a `2` is a merge commit, and the messages say
    /// whether a rewritten branch kept its history.
    fn commits_ahead(&self, base: &str, head: &str) -> Vec<String> {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");
        let base = repo
            .revparse_single(&format!("refs/heads/{base}"))
            .expect("the base branch")
            .id();
        let head = repo
            .revparse_single(&format!("refs/heads/{head}"))
            .expect("the head branch")
            .id();

        let mut walk = repo.revwalk().expect("a revwalk");
        walk.push(head).expect("pushing the head");
        walk.hide(base).expect("hiding the base");
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .expect("sorting");

        walk.map(|oid| {
            let commit = repo
                .find_commit(oid.expect("a commit id"))
                .expect("a commit");

            format!(
                "{} {}",
                commit.parent_count(),
                commit.summary().unwrap_or_default()
            )
        })
        .collect()
    }

    /// The files a pull request from `base` to `head` shows as changed.
    ///
    /// GitHub works this out against the merge base of the two branches, so that
    /// is what this does — it is the assertion the end-to-end suite makes with
    /// `pulls/{n}/files`, and the one that says whether a stacked branch is
    /// still measured against the branch below rather than against something
    /// further down.
    fn files_changed(&self, base: &str, head: &str) -> Vec<String> {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");
        let base = repo
            .revparse_single(&format!("refs/heads/{base}"))
            .expect("the base branch")
            .id();
        let head = repo
            .revparse_single(&format!("refs/heads/{head}"))
            .expect("the head branch")
            .id();
        let merge_base = repo.merge_base(base, head).expect("a merge base");

        let old = repo
            .find_commit(merge_base)
            .expect("the merge base commit")
            .tree()
            .expect("its tree");
        let new = repo
            .find_commit(head)
            .expect("the head commit")
            .tree()
            .expect("its tree");

        let diff = repo
            .diff_tree_to_tree(Some(&old), Some(&new), None)
            .expect("a diff");

        let mut files: Vec<String> = diff
            .deltas()
            .filter_map(|delta| {
                delta
                    .new_file()
                    .path()
                    .map(|path| path.to_string_lossy().to_string())
            })
            .collect();
        files.sort();
        files
    }

    fn tip(&self, branch: &str) -> git2::Oid {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");

        repo.find_reference(&format!("refs/heads/{branch}"))
            .expect("the branch")
            .target()
            .expect("its target")
    }

    /// Whether `descendant` has `ancestor` in its history, which is the
    /// difference between a branch that was written past and one that was
    /// rewritten.
    fn descends_from(&self, descendant: git2::Oid, ancestor: git2::Oid) -> bool {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");

        descendant == ancestor
            || repo
                .graph_descendant_of(descendant, ancestor)
                .expect("an ancestry answer")
    }

    /// Every branch on the remote under the branch prefix, sorted.
    fn spr_branches(&self) -> Vec<String> {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");

        let mut branches: Vec<String> = repo
            .branches(Some(git2::BranchType::Local))
            .expect("the branches")
            .filter_map(|branch| {
                let (branch, _) = branch.ok()?;
                let name = branch.name().ok()??.to_string();

                name.starts_with(BRANCH_PREFIX).then_some(name)
            })
            .collect();

        branches.sort();
        branches
    }

    fn has_branch(&self, branch: &str) -> bool {
        let repo = git2::Repository::open(&self.remote).expect("the remote repository");

        repo.find_reference(&format!("refs/heads/{branch}")).is_ok()
    }
}

/// The options `jj spr diff <args>` would run with.
///
/// Parsed rather than built, because `DiffOptions` is what the command line hands
/// over and a test that constructed one by hand could ask for a combination the
/// parser refuses.
fn diff_options(args: &[&str]) -> DiffOptions {
    use clap::Parser;

    let mut argv = vec!["diff"];
    argv.extend_from_slice(args);

    DiffOptions::try_parse_from(argv).expect("the options should parse")
}

fn slug(title: &str) -> String {
    title.replace(' ', "-")
}

fn describe(title: &str) -> String {
    format!("{title}\n\nSummary:\nthe summary of {title}.")
}

// ---------------------------------------------------------------------------
// The tests

/// The harness itself: a push reaches the bare repository, and the fake opens a
/// pull request for it based on the default branch.
///
/// Everything below rests on this much working, and nothing else asserts it.
#[tokio::test]
async fn the_harness_pushes_a_branch_and_opens_a_pull_request() {
    let local = Local::new();
    let config = local.config(BaseStrategy::Synthetic, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["one change"]);
    local
        .diff(&gh, &config)
        .await
        .expect("the diff should succeed");

    let branch = gh.head_of(1);
    assert_eq!(local.spr_branches(), vec![branch.clone()]);
    assert_eq!(gh.base_of(1), MASTER);
    assert_eq!(
        local.commits_ahead(MASTER, &branch),
        vec!["1 one change".to_string()]
    );
    assert_eq!(
        local.files_changed(MASTER, &branch),
        vec![slug("one change")]
    );
}

/// Under `spr.baseStrategy = linear-rebase` every pull request branch is a chain
/// of single-parent commits on the branch below it, and no base branches are
/// generated at all.
#[tokio::test]
async fn a_linear_rebase_stack_is_a_chain_of_single_parent_commits() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["rebase bottom", "rebase top"]);
    local
        .diff(&gh, &config)
        .await
        .expect("the diff should succeed");

    let (bottom, top) = (gh.head_of(1), gh.head_of(2));

    assert_eq!(gh.base_of(1), MASTER, "the bottom is on the default branch");
    assert_eq!(
        gh.base_of(2),
        bottom,
        "the top should be based on the branch of the one below"
    );
    assert_eq!(
        local.commits_ahead(MASTER, &bottom),
        vec!["1 rebase bottom".to_string()]
    );
    assert_eq!(
        local.commits_ahead(&bottom, &top),
        vec!["1 rebase top".to_string()],
        "the top's branch should be one ordinary commit on the branch below it"
    );

    let mut expected = vec![bottom, top];
    expected.sort();
    assert_eq!(
        local.spr_branches(),
        expected,
        "the strategy should have pushed the two head branches and nothing else"
    );
}

/// Amending the bottom of a `linear-rebase` stack replays the commits of the
/// pull request above onto the new tip of the branch below: rewriting them, which
/// is what the strategy trades away, while keeping their number and their
/// messages, which is what it keeps.
///
/// The files the pull request above shows are the assertion that the replay
/// landed on the right base — a branch left behind on the old tip would show the
/// change below as well as its own.
#[tokio::test]
async fn amending_below_a_linear_rebase_pull_request_replays_its_commits() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["replay bottom", "replay top"]);
    local.diff(&gh, &config).await.expect("the first push");

    let (bottom, top) = (gh.head_of(1), gh.head_of(2));

    // A second round of the top change, so that there is review history to lose.
    local.write("replay top", "a second round");
    local
        .diff_with(&gh, &config, &["-r", "@", "-m", "the second round"])
        .await
        .expect("the second round");
    assert_eq!(
        local.commits_ahead(&bottom, &top),
        vec!["1 replay top".to_string(), "1 the second round".to_string()],
        "the amend should have added a second commit to the top's branch"
    );

    let (before_bottom, before_top) = (local.tip(&bottom), local.tip(&top));

    // Amend the change at the bottom, which is what the one above is based on.
    run("jj", &["edit", "@-"], &local.repo);
    local.write("replay bottom", "amended bottom content");
    run("jj", &["edit", "@+"], &local.repo);
    local
        .amend(&gh, &config, "amend")
        .await
        .expect("the amending push");

    assert!(
        local.descends_from(local.tip(&bottom), before_bottom),
        "nothing moved under the bottom, so its own branch should have moved forward"
    );
    assert!(
        !local.descends_from(local.tip(&top), before_top),
        "the top's branch should have been rewritten rather than added to, which is what \
         this strategy gives up"
    );
    assert_eq!(
        local.commits_ahead(&bottom, &top),
        vec!["1 replay top".to_string(), "1 the second round".to_string()],
        "both rounds should have come along, still one parent each"
    );
    assert_eq!(
        local.files_changed(&bottom, &top),
        vec![slug("replay top")],
        "the top should still be reviewing its own change and nothing else"
    );
    assert_eq!(
        gh.base_of(2),
        bottom,
        "the base should not have moved, so no stack would have to be dissolved"
    );
    assert!(
        !gh.calls()
            .iter()
            .any(|call| matches!(call, Call::Retargeted { .. })),
        "a replay retargets nothing: {:?}",
        gh.calls()
    );
}

/// A branch pushed under one of the merging strategies is rebuilt as a chain the
/// next time it is pushed under `linear-rebase`.
///
/// The review history cannot come with it — a merge commit says nothing about
/// which of its ancestors were rounds of this change — so what this pins is that
/// the branch does not stay merge-shaped.
#[tokio::test]
async fn switching_to_linear_rebase_rebuilds_a_merge_shaped_branch() {
    let local = Local::new();
    let linear = local.config(BaseStrategy::Linear, StackDisplay::None);
    let gh = local.github(&linear);

    local.stack(&["migrate bottom", "migrate top"]);
    local.diff(&gh, &linear).await.expect("the push");

    let (bottom, top) = (gh.head_of(1), gh.head_of(2));

    // Amend the bottom under `linear`, so the branch above gains the merge
    // commit that strategy brings the new base in with.
    run("jj", &["edit", "@-"], &local.repo);
    local.write("migrate bottom", "amended once");
    run("jj", &["edit", "@+"], &local.repo);
    local.amend(&gh, &linear, "amend").await.expect("the amend");

    assert!(
        local
            .commits_ahead(&bottom, &top)
            .iter()
            .any(|commit| commit.starts_with("2 ")),
        "the linear strategy should have left a merge commit to migrate away from: {:?}",
        local.commits_ahead(&bottom, &top)
    );

    let rebasing = local.config(BaseStrategy::LinearRebase, StackDisplay::None);
    run("jj", &["edit", "@-"], &local.repo);
    local.write("migrate bottom", "amended twice");
    run("jj", &["edit", "@+"], &local.repo);
    local
        .amend(&gh, &rebasing, "amend again")
        .await
        .expect("the migrating push");

    for commit in local.commits_ahead(&bottom, &top) {
        assert!(
            commit.starts_with("1 "),
            "the branch should have been rebuilt without merge commits: {commit}"
        );
    }
    assert_eq!(
        local.files_changed(&bottom, &top),
        vec![slug("migrate top")],
        "the top should still be reviewing its own change and nothing else"
    );
}

/// Under `spr.baseStrategy = linear` a branch only ever moves forward: every
/// commit put on it descends from what was there before, so the push needs no
/// force and GitHub keeps the review comments on every commit.
#[tokio::test]
async fn amending_below_a_linear_pull_request_only_moves_branches_forward() {
    let local = Local::new();
    let config = local.config(BaseStrategy::Linear, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["forward bottom", "forward top"]);
    local.diff(&gh, &config).await.expect("the push");

    let branches = [gh.head_of(1), gh.head_of(2)];
    let before: Vec<git2::Oid> = branches.iter().map(|branch| local.tip(branch)).collect();

    run("jj", &["edit", "@-"], &local.repo);
    local.write("forward bottom", "amended content");
    run("jj", &["edit", "@+"], &local.repo);
    local.amend(&gh, &config, "amend").await.expect("the amend");

    for (branch, before) in branches.iter().zip(&before) {
        let after = local.tip(branch);
        assert!(
            local.descends_from(after, *before),
            "{branch} was rewritten rather than moved forward"
        );
        assert_ne!(after, *before, "{branch} should have moved");
    }
}

/// Under `spr.baseStrategy = linear` a stacked pull request is based on the
/// branch of the one below, and no base branch is generated.
#[tokio::test]
async fn a_linear_stack_bases_each_pull_request_on_the_one_below() {
    let local = Local::new();
    let config = local.config(BaseStrategy::Linear, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["linear bottom", "linear top"]);
    local.diff(&gh, &config).await.expect("the push");

    let (bottom, top) = (gh.head_of(1), gh.head_of(2));
    assert_eq!(gh.base_of(1), MASTER);
    assert_eq!(gh.base_of(2), bottom);

    let mut expected = vec![bottom, top];
    expected.sort();
    assert_eq!(
        local.spr_branches(),
        expected,
        "no base branches should have been generated"
    );
}

/// Under `spr.baseStrategy = synthetic` a stacked pull request gets a base branch
/// of its own, carrying the parent change's tree.
#[tokio::test]
async fn a_synthetic_stack_gives_each_pull_request_a_base_branch() {
    let local = Local::new();
    let config = local.config(BaseStrategy::Synthetic, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["synthetic bottom", "synthetic top"]);
    local.diff(&gh, &config).await.expect("the push");

    let base_of_top = gh.base_of(2);
    assert!(
        base_of_top.starts_with(&format!("{BRANCH_PREFIX}{MASTER}.")),
        "the top should be based on a generated base branch, not {base_of_top}"
    );
    assert!(
        local.has_branch(&base_of_top),
        "the generated base branch should be on the remote"
    );
    assert_eq!(
        local.files_changed(&base_of_top, &gh.head_of(2)),
        vec![slug("synthetic top")],
        "the pull request should be reviewing its own change against its base branch"
    );
}

/// Adopting `spr.baseStrategy = linear` on a stack that already has pull requests
/// moves the one above onto the branch below and takes the base branch it left
/// away — in that order, since a base branch deleted while a pull request still
/// points at it closes that pull request.
#[tokio::test]
async fn migrating_a_stack_to_linear_retargets_it_and_takes_away_its_base_branch() {
    let local = Local::new();
    let synthetic = local.config(BaseStrategy::Synthetic, StackDisplay::None);
    let gh = local.github(&synthetic);

    local.stack(&["migrate2 bottom", "migrate2 top"]);
    local.diff(&gh, &synthetic).await.expect("the first push");

    let generated = gh.base_of(2);
    assert!(local.has_branch(&generated));

    let linear = local.config(BaseStrategy::Linear, StackDisplay::None);
    local.write("migrate2 top", "amended so it needs a push");
    local
        .amend(&gh, &linear, "amend")
        .await
        .expect("the migrating push");

    let bottom = gh.head_of(1);
    assert_eq!(
        gh.base_of(2),
        bottom,
        "the top should have moved onto the branch below"
    );
    assert!(
        !local.has_branch(&generated),
        "the base branch it left should have been taken away: {generated}"
    );

    // The order, which only the call log can show: the retarget comes first, and
    // the deletion is part of it — a branch deleted before the retarget would
    // have closed the pull request.
    assert_eq!(
        gh.calls()
            .iter()
            .filter(|call| matches!(call, Call::Retargeted { .. }))
            .collect::<Vec<_>>(),
        vec![&Call::Retargeted {
            number: 2,
            to: bottom
        }]
    );
}

/// Under `spr.stackDisplay = github` the pull requests a run pushes are registered as one
/// stack, bottom first.
///
/// What the fake checks is what GitHub checks: that the pull requests chain
/// base-to-head. So this asserts jj-spr built a chain GitHub would accept, and
/// registered it in one call.
#[tokio::test]
async fn a_run_registers_its_pull_requests_as_one_stack() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::Github);
    let gh = local.github(&config);

    local.stack(&["stacked bottom", "stacked middle", "stacked top"]);
    local.diff(&gh, &config).await.expect("the push");

    let stacks = gh.open_stacks();
    assert_eq!(stacks.len(), 1, "one run should register one stack");
    assert_eq!(
        stacks[0].members,
        vec![1, 2, 3],
        "the stack should hold the run's pull requests, bottom first"
    );
    assert!(
        gh.calls()
            .iter()
            .any(|call| matches!(call, Call::StackCreated { .. })),
        "the run should have created the stack: {:?}",
        gh.calls()
    );
}

/// A single change is no stack: GitHub's stacks take at least two chained pull
/// requests, so a run that pushes one registers nothing at all.
#[tokio::test]
async fn a_single_pull_request_is_not_registered_as_a_stack() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::Github);
    let gh = local.github(&config);

    local.stack(&["alone"]);
    local.diff(&gh, &config).await.expect("the push");

    assert!(
        gh.open_stacks().is_empty(),
        "a run of one should register no stack"
    );
    assert!(
        !gh.calls()
            .iter()
            .any(|call| matches!(call, Call::StackCreated { .. })),
        "nothing should have been sent to the stacks API: {:?}",
        gh.calls()
    );
}

/// A run that finds nothing to do does nothing: the branches stay exactly where
/// they are, and no pull request is touched.
///
/// Worth pinning under `spr.baseStrategy = linear-rebase` above all, because that
/// is the strategy with something to get wrong here — a branch it rebuilds is
/// force-pushed, so a run that decided it had work to do when it did not would
/// rewrite a branch and notify every reviewer for nothing.
///
/// What this does *not* establish is that the rebuild is repeatable. Deliberately
/// checked twice over: a run with nothing to do never gets as far as rebuilding
/// anything, so a rebuild that came out differently every time would slip past
/// this test — `replaying_twice_gives_the_same_commits`, in `replay`, is what
/// pins that.
#[tokio::test]
async fn a_second_run_with_nothing_to_do_changes_nothing() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::None);
    let gh = local.github(&config);

    local.stack(&["idempotent bottom", "idempotent top"]);
    local.diff(&gh, &config).await.expect("the first push");

    let branches = [gh.head_of(1), gh.head_of(2)];
    let before: Vec<git2::Oid> = branches.iter().map(|branch| local.tip(branch)).collect();
    let calls_before = gh.calls().len();

    local.diff(&gh, &config).await.expect("the second push");

    for (branch, before) in branches.iter().zip(&before) {
        assert_eq!(
            local.tip(branch),
            *before,
            "{branch} moved on a run that had nothing to do"
        );
    }
    assert_eq!(
        gh.calls().len(),
        calls_before,
        "a run with nothing to do should not touch a pull request: {:?}",
        &gh.calls()[calls_before..]
    );
}

/// Moving a stacked pull request's base takes it out of its GitHub stack first.
///
/// GitHub owns a stacked pull request's base ref and refuses any update carrying
/// one, so the order is forced: dissolve, then retarget. The fake refuses it too —
/// that rule is pinned live by
/// `retargeting_a_pull_request_in_a_github_stack_takes_it_out_of_the_stack` — so a
/// run that got the order wrong fails here rather than merely looking odd.
///
/// Abandoning the change below is what makes the base move: the change above then
/// sits directly on the trunk, which is what its pull request has to be against.
#[tokio::test]
async fn moving_a_stacked_pull_requests_base_unstacks_it_first() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::Github);
    let gh = local.github(&config);

    local.stack(&["interlock bottom", "interlock top"]);
    local.diff(&gh, &config).await.expect("the first push");
    assert_eq!(
        gh.open_stacks().len(),
        1,
        "the run should have registered a stack to be taken apart"
    );

    run("jj", &["abandon", "@-"], &local.repo);
    local
        .diff_with(&gh, &config, &["-r", "@", "-m", "now on the trunk"])
        .await
        .expect("the retargeting push");

    assert_eq!(
        gh.base_of(2),
        MASTER,
        "the pull request should now be against the default branch"
    );

    let calls = gh.calls();
    let unstacked = calls
        .iter()
        .position(|call| matches!(call, Call::Unstacked { .. }))
        .expect("the stack should have been dissolved");
    let retargeted = calls
        .iter()
        .position(|call| matches!(call, Call::Retargeted { .. }))
        .expect("the pull request should have been retargeted");
    assert!(
        unstacked < retargeted,
        "the stack has to be dissolved before the base moves: {calls:?}"
    );
    assert!(
        gh.open_stacks().is_empty(),
        "the stack should be gone, since only `diff` registers one and this run has \
         nothing left to chain"
    );
}

/// A dry run reports and does nothing: no branch on the remote, no pull request,
/// no stack.
#[tokio::test]
async fn a_dry_run_pushes_nothing_and_opens_nothing() {
    let local = Local::new();
    let config = local.config(BaseStrategy::LinearRebase, StackDisplay::Github);
    let gh = local.github(&config);

    local.stack(&["dry bottom", "dry top"]);
    local
        .diff_with(&gh, &config, &["--all", "-r", "trunk()..@", "--dry-run"])
        .await
        .expect("the dry run");

    assert!(
        local.spr_branches().is_empty(),
        "a dry run should push no branch: {:?}",
        local.spr_branches()
    );
    assert!(
        gh.calls().is_empty(),
        "a dry run should change nothing on GitHub: {:?}",
        gh.calls()
    );
}
