/*
 * End-to-end tests against a real GitHub repository.
 *
 * These are the only tests that exercise the whole path a stacked PR takes:
 * pushing branches, opening pull requests, and reading their bodies back off
 * GitHub. Everything else in the suite stops at the library boundary.
 *
 * They are skipped unless E2E_TEST_REPO names a repository you are content for
 * them to open and close pull requests in:
 *
 *     E2E_TEST_REPO=https://github.com/you/scratch \
 *         cargo test --test github_e2e_test -- --test-threads=1
 *
 * When it is set, the rest of the configuration must be complete: anything
 * missing fails rather than skips, so a misconfigured run cannot be mistaken
 * for a passing one.
 *
 * Every pull request and branch they create is cleaned up, including when a
 * test fails part-way through.
 */

use std::process::Command;

use jj_spr::message::{MessageSection, parse_message};

/// The repository under test, from `E2E_TEST_REPO`.
struct Target {
    owner: String,
    repo: String,
    url: String,
}

/// Read the target repository, or `None` when these tests are not enabled.
///
/// A value that is set but unusable is a failure, not a skip: a run that was
/// asked for must not quietly do nothing.
fn target() -> Option<Target> {
    let url = std::env::var("E2E_TEST_REPO").ok()?;

    let path = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit("github.com")
        .next()
        .unwrap_or_default()
        .trim_start_matches([':', '/']);
    let (owner, repo) = path
        .split_once('/')
        .unwrap_or_else(|| panic!("E2E_TEST_REPO must be a GitHub repository URL, got {url:?}"));
    assert!(
        !owner.is_empty() && !repo.is_empty(),
        "E2E_TEST_REPO must be a GitHub repository URL, got {url:?}"
    );

    Some(Target {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        url: url.clone(),
    })
}

/// A GitHub token with access to the target repository.
fn token() -> String {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        return token;
    }

    let out = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .expect("E2E_TEST_REPO is set, so a token is needed: install `gh` or set GITHUB_TOKEN");
    assert!(
        out.status.success(),
        "E2E_TEST_REPO is set, so a token is needed: run `gh auth login` or set GITHUB_TOKEN"
    );

    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn run(program: &str, args: &[&str], dir: &std::path::Path) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn jj_spr(args: &[&str], dir: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_jj-spr"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run jj-spr");
    assert!(
        out.status.success(),
        "jj-spr {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Run jj-spr and hand back everything it said, `Err` when it refused.
///
/// [`jj_spr`] panics with jj-spr's own output, which is what nearly every test
/// wants. A test whose subject is *why* a command might be refused needs the
/// failure as a value instead, so that it can be reported alongside what GitHub
/// was saying at the time — which is the only thing that tells the two possible
/// causes apart.
fn try_jj_spr(args: &[&str], dir: &std::path::Path) -> Result<String, String> {
    let out = Command::new(env!("CARGO_BIN_EXE_jj-spr"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run jj-spr");

    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    if out.status.success() {
        Ok(said)
    } else {
        Err(said)
    }
}

/// A clone of the target repository, configured for spr, that closes whatever
/// it opened when it goes out of scope.
struct Scratch {
    target: Target,
    dir: tempfile::TempDir,
    prefix: String,
    /// Stacks to take apart when this goes out of scope.
    ///
    /// A stack outlives the pull requests in it — closing them all leaves it
    /// open — so closing the pull requests is not enough to clean one up, and
    /// a stack left behind in the target repository is what makes a later run
    /// find its pull requests already stacked. Recorded rather than unstacked
    /// at the end of a test so that a failed assertion cleans up too.
    stacks: std::cell::RefCell<Vec<u64>>,
}

impl Scratch {
    /// `tag` distinguishes concurrent runs: it names the branch prefix, so two
    /// tests cannot collide on a branch even against the same repository.
    fn new(target: Target, tag: &str) -> Self {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path();
        let prefix = format!("spr-e2e-{tag}-{}/", std::process::id());

        run(
            "jj",
            &["git", "clone", "--colocate", &target.url, "."],
            path,
        );
        run(
            "jj",
            &["config", "set", "--repo", "user.email", "e2e@example.com"],
            path,
        );
        run(
            "jj",
            &["config", "set", "--repo", "user.name", "jj-spr e2e"],
            path,
        );

        for (key, value) in [
            (
                "spr.githubRepository",
                format!("{}/{}", target.owner, target.repo),
            ),
            ("spr.githubAuthToken", token()),
            ("spr.githubRemoteName", "origin".to_owned()),
            ("spr.branchPrefix", prefix.clone()),
            ("spr.requireTestPlan", "false".to_owned()),
        ] {
            run("git", &["config", key, &value], path);
        }

        Self {
            target,
            dir,
            prefix,
            stacks: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Set one jj-spr setting, for the tests that are about a setting.
    fn set_config(&self, key: &str, value: &str) {
        run("git", &["config", key, value], self.path());
    }

    /// Stack `titles` on the trunk, one change each, and push them all.
    ///
    /// Returns each change's PR number, bottom-up — read back from the commit
    /// messages jj-spr rewrote, which is where it records them.
    fn push_stack(&self, titles: &[&str]) -> Vec<u64> {
        run(
            "jj",
            &["new", "trunk()", "-m", &describe(titles[0])],
            self.path(),
        );
        std::fs::write(self.path().join(slug(titles[0])), titles[0]).unwrap();
        for title in &titles[1..] {
            run("jj", &["new", "-m", &describe(title)], self.path());
            std::fs::write(self.path().join(slug(title)), title).unwrap();
        }

        jj_spr(&["diff", "--all", "-r", "trunk()..@"], self.path());
        self.pr_numbers(titles.len())
    }

    /// The stack's PR numbers, bottom-up.
    fn pr_numbers(&self, expected: usize) -> Vec<u64> {
        let numbers: Vec<u64> = run(
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                "trunk()..@",
                "--template",
                r#"description ++ "\n\x1e""#,
            ],
            self.path(),
        )
        .split('\x1e')
        .filter(|chunk| !chunk.trim().is_empty())
        .filter_map(|chunk| {
            parse_message(chunk, MessageSection::Title)
                .get(&MessageSection::PullRequest)?
                .rsplit('/')
                .next()?
                .parse()
                .ok()
        })
        .collect();

        // jj log is newest-first; the stack reads bottom-up.
        let numbers: Vec<u64> = numbers.into_iter().rev().collect();
        assert_eq!(
            numbers.len(),
            expected,
            "every change should have recorded a PR number"
        );

        numbers
    }

    /// One field of a pull request, as `jq` selects it.
    fn pr_field(&self, number: u64, jq: &str) -> String {
        run(
            "gh",
            &[
                "api",
                &format!(
                    "repos/{}/{}/pulls/{number}",
                    self.target.owner, self.target.repo
                ),
                "--jq",
                jq,
            ],
            self.path(),
        )
    }

    /// `"open"` or `"closed"`. A merged pull request reads as closed.
    fn pr_state(&self, number: u64) -> String {
        self.pr_field(number, ".state")
    }

    /// The branch the pull request is asking to be merged into.
    fn pr_base_branch(&self, number: u64) -> String {
        self.pr_field(number, ".base.ref")
    }

    /// The branch the pull request is asking to have merged.
    fn pr_head_branch(&self, number: u64) -> String {
        self.pr_field(number, ".head.ref")
    }

    fn pr_head_sha(&self, number: u64) -> String {
        self.pr_field(number, ".head.sha")
    }

    /// How `head` stands to `base` on GitHub: `"ahead"` when `head` descends
    /// from `base`, `"identical"` when they are the same commit, and
    /// `"behind"` or `"diverged"` when a branch has been rewritten.
    fn compare(&self, base: &str, head: &str) -> String {
        let path = format!(
            "repos/{}/{}/compare/{base}...{head}",
            self.target.owner, self.target.repo
        );

        run("gh", &["api", &path, "--jq", ".status"], self.path())
    }

    /// Every branch this run has put on the remote, by name, sorted.
    fn remote_branches(&self) -> Vec<String> {
        let listing = run(
            "git",
            &[
                "ls-remote",
                "--heads",
                "origin",
                &format!("{}*", self.prefix),
            ],
            self.path(),
        );

        let mut branches: Vec<String> = ls_remote_refs(&listing)
            .map(|r| r.trim_start_matches("refs/heads/").to_owned())
            .collect();

        branches.sort();
        branches
    }

    /// What GitHub reports about merging pull request `number`: the `mergeable`
    /// verdict and the `mergeStateStatus` that `github::merge_requirements`
    /// reads, on one line.
    ///
    /// GitHub works both out lazily and sends them back to `UNKNOWN` whenever
    /// the pull request changes, so this waits for an answer rather than
    /// reporting the `UNKNOWN` a first ask gets.
    fn pr_merge_state(&self, number: u64) -> String {
        let query = "query($owner:String!,$repo:String!,$number:Int!){\
                     repository(owner:$owner,name:$repo){\
                     pullRequest(number:$number){mergeable mergeStateStatus}}}";

        let mut answer = String::new();
        for _ in 0..10 {
            answer = run(
                "gh",
                &[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}"),
                    "-F",
                    &format!("owner={}", self.target.owner),
                    "-F",
                    &format!("repo={}", self.target.repo),
                    "-F",
                    &format!("number={number}"),
                    "--jq",
                    r#".data.repository.pullRequest
                       | "mergeable=\(.mergeable) mergeStateStatus=\(.mergeStateStatus)""#,
                ],
                self.path(),
            );

            if !answer.contains("UNKNOWN") {
                return answer;
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        answer
    }

    /// The commit the default branch is at right now.
    fn default_branch_sha(&self) -> String {
        run(
            "gh",
            &[
                "api",
                &format!(
                    "repos/{}/{}/commits/{}",
                    self.target.owner,
                    self.target.repo,
                    self.default_branch()
                ),
                "--jq",
                ".sha",
            ],
            self.path(),
        )
    }

    /// The commits the default branch has gained since `before`, oldest first.
    fn commits_landed_since(&self, before: &str) -> Vec<String> {
        let path = format!(
            "repos/{}/{}/compare/{before}...{}",
            self.target.owner,
            self.target.repo,
            self.default_branch()
        );

        run("gh", &["api", &path, "--jq", ".commits[].sha"], self.path())
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The files one commit on the remote changes, sorted.
    fn commit_files(&self, sha: &str) -> Vec<String> {
        let path = format!(
            "repos/{}/{}/commits/{sha}",
            self.target.owner, self.target.repo
        );

        let mut files: Vec<String> = run(
            "gh",
            &["api", &path, "--jq", ".files[].filename"],
            self.path(),
        )
        .lines()
        .map(str::to_owned)
        .collect();

        files.sort();
        files
    }

    /// The repository's default branch, which is the branch jj-spr retargets a
    /// pull request at once its commit sits directly on it.
    fn default_branch(&self) -> String {
        run(
            "gh",
            &[
                "api",
                &format!("repos/{}/{}", self.target.owner, self.target.repo),
                "--jq",
                ".default_branch",
            ],
            self.path(),
        )
    }

    fn remote_has_branch(&self, branch: &str) -> bool {
        !run(
            "git",
            &["ls-remote", "--heads", "origin", branch],
            self.path(),
        )
        .is_empty()
    }

    fn repo_arg(&self) -> String {
        format!("{}/{}", self.target.owner, self.target.repo)
    }

    /// The open stack pull request `number` is in, if any.
    ///
    /// Filtered on `open`, as everything that acts on a stack has to be:
    /// GitHub keeps a stack for ever once it holds a merged pull request and
    /// goes on listing it here.
    fn open_stack_for(&self, number: u64) -> Option<StackOnGitHub> {
        let listing = run(
            "gh",
            &[
                "api",
                &format!(
                    "repos/{}/{}/stacks?pull_request={number}",
                    self.target.owner, self.target.repo
                ),
                "--jq",
                r#".[] | select(.open) | "\(.number) \([.pull_requests[].number] | join(","))""#,
            ],
            self.path(),
        );

        let line = listing.lines().next().filter(|l| !l.trim().is_empty())?;
        let (stack_number, members) = line.split_once(' ').unwrap_or((line, ""));
        let number: u64 = stack_number.parse().expect("a stack number");

        // Noted for teardown as soon as it is seen, so that an assertion
        // failing below this line still leaves the repository clean.
        self.stacks.borrow_mut().push(number);

        Some(StackOnGitHub {
            number,
            pull_requests: members
                .split(',')
                .filter(|m| !m.is_empty())
                .map(|m| m.parse().expect("a pull request number"))
                .collect(),
        })
    }
}

/// A stack as GitHub holds it: its number, and its members bottom to top.
struct StackOnGitHub {
    number: u64,
    pull_requests: Vec<u64>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Stacks first: a pull request cannot be retargeted while it is in one,
        // and a stack goes on holding pull requests that have been closed, so
        // leaving this until after the closing below would leave a live stack
        // in the repository for good.
        for number in self.stacks.borrow().iter() {
            let _ = Command::new("gh")
                .args([
                    "api",
                    "--method",
                    "POST",
                    &format!(
                        "repos/{}/{}/stacks/{number}/unstack",
                        self.target.owner, self.target.repo
                    ),
                ])
                .current_dir(self.path())
                .output();
        }

        // Close everything this run opened, whether or not the test passed —
        // a failed test must not leave pull requests behind.
        let open = Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                &format!("{}/{}", self.target.owner, self.target.repo),
                "--state",
                "open",
                "--limit",
                "100",
                "--json",
                "number,headRefName",
                "--jq",
                &format!(
                    r#".[] | select(.headRefName | startswith("{}")) | .number"#,
                    self.prefix
                ),
            ])
            .current_dir(self.path())
            .output();

        if let Ok(out) = open {
            for number in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                let _ = Command::new("gh")
                    .args([
                        "pr",
                        "close",
                        number,
                        "--repo",
                        &format!("{}/{}", self.target.owner, self.target.repo),
                        "--delete-branch",
                    ])
                    .current_dir(self.path())
                    .output();
            }
        }

        // Base branches are not attached to a PR, so close --delete-branch does
        // not reach them.
        let refs = Command::new("git")
            .args([
                "ls-remote",
                "--heads",
                "origin",
                &format!("{}*", self.prefix),
            ])
            .current_dir(self.path())
            .output();

        if let Ok(out) = refs {
            let listing = String::from_utf8_lossy(&out.stdout);
            let branches: Vec<String> = ls_remote_refs(&listing).map(str::to_owned).collect();
            for branch in branches {
                let _ = Command::new("git")
                    .args(["push", "origin", "--delete", &branch])
                    .current_dir(self.path())
                    .output();
            }
        }
    }
}

fn slug(title: &str) -> String {
    title.replace(' ', "-")
}

/// The refs named by `git ls-remote` output, one per line as `<sha>\t<ref>`.
fn ls_remote_refs(listing: &str) -> impl Iterator<Item = &str> {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
}

/// A tag that tells this run's changes apart from every earlier run's.
///
/// The two tests that merge a pull request need it. What they merge stays on the
/// default branch, so a later run pushing the same file with the same content
/// would produce a change that does nothing — and a stack whose bottom is empty
/// is no stack at all: the change above it is already on the default branch, so
/// it never gets the base branch these tests are about.
fn run_tag() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock should be past the epoch")
        .as_secs();

    format!("{}-{seconds}", std::process::id())
}

fn describe(title: &str) -> String {
    format!("{title}\n\nSummary:\nthe summary of {title}.")
}

/// The harness reaches GitHub and puts back what it took.
///
/// Every test below rests on this much working, and nothing else asserts it:
/// a failure here says the clone, the configuration or the token is wrong,
/// rather than that the behaviour under test is.
#[test]
fn the_harness_opens_a_stack_and_cleans_up_after_itself() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "harness");

    let prs = scratch.push_stack(&["e2e harness bottom", "e2e harness top"]);

    assert_eq!(
        prs.len(),
        2,
        "each change should have opened a pull request"
    );
    for number in &prs {
        assert_eq!(
            scratch.pr_state(*number),
            "open",
            "PR #{number} should be open"
        );
    }
}

/// Landing the bottom of a stack retargets the pull request above it at the
/// default branch and takes away the base branch it used to point at.
///
/// The order matters and only GitHub can show it: a base branch deleted while
/// the pull request still targets it makes GitHub close that pull request,
/// review and all. So the state is asserted as well as the base — were the
/// deletion to run first again, the base and the branch would look right while
/// the pull request sat closed.
#[test]
fn landing_below_a_pull_request_retargets_it_and_leaves_it_open() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "landretarget");

    let tag = run_tag();
    let (bottom_title, top_title) = (
        format!("e2e land bottom {tag}"),
        format!("e2e land top {tag}"),
    );
    let prs = scratch.push_stack(&[&bottom_title, &top_title]);
    let (bottom, top) = (prs[0], prs[1]);

    let old_base = scratch.pr_base_branch(top);
    assert!(
        old_base.starts_with(&scratch.prefix),
        "the top PR should be stacked on a base branch jj-spr made, got {old_base:?}"
    );

    jj_spr(&["land", "-r", "@-"], scratch.path());

    assert_eq!(
        scratch.pr_field(bottom, ".merged"),
        "true",
        "PR #{bottom} should have been merged"
    );
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "landing below PR #{top} closed it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.default_branch(),
        "PR #{top} should now target the default branch"
    );
    assert!(
        !scratch.remote_has_branch(&old_base),
        "the base branch PR #{top} left behind is still on the remote: {old_base}"
    );
}

/// The same has to hold for a pull request the *local* repository cannot see.
///
/// `land` reads the local change stack to find what is stacked on the pull
/// request it is landing, so a change that is not there — abandoned locally, or
/// living in a workspace this one has not fetched — is invisible to it. Under
/// `spr.baseStrategy = linear` that pull request is based on the very head
/// branch `land` deletes on its way out.
///
/// Run against a real repository with the GitHub half of the lookup removed,
/// this fails on the base branch rather than on the state: the pull request is
/// left open, still pointing at the deleted head branch. So the assertion that
/// matters here is the retargeting, not the survival — GitHub does eventually
/// retarget such a pull request itself, but asynchronously, after `land` has
/// returned, and racing the branch deletion `land` has already started.
///
/// Only GitHub knows such a pull request is there, which is why `land` asks it
/// as well as jj. Only an end-to-end test can show it: the whole point is a
/// pull request that exists on GitHub and nowhere else.
#[test]
fn landing_below_a_pull_request_jj_cannot_see_leaves_it_open() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "landunseen");
    scratch.set_config("spr.baseStrategy", "linear");

    let tag = run_tag();
    let (bottom_title, top_title) = (
        format!("e2e unseen bottom {tag}"),
        format!("e2e unseen top {tag}"),
    );
    let prs = scratch.push_stack(&[&bottom_title, &top_title]);
    let (bottom, top) = (prs[0], prs[1]);

    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.pr_head_branch(bottom),
        "under the linear strategy PR #{top} should be based on PR #{bottom}'s head branch"
    );

    // Take the top change out of the local repository. Everything jj could have
    // told `land` about PR #top goes with it, leaving the pull request itself
    // untouched on GitHub — which is the situation under test.
    run("jj", &["abandon", "@"], scratch.path());

    jj_spr(&["land", "-r", "@-"], scratch.path());

    assert_eq!(
        scratch.pr_field(bottom, ".merged"),
        "true",
        "PR #{bottom} should have been merged"
    );
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "landing below PR #{top} closed it, even though GitHub could say it was there"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.default_branch(),
        "PR #{top} should have been retargeted at the default branch"
    );
}

/// The same has to hold when the pull request below was merged on GitHub rather
/// than by `jj spr land`: there was no land to retarget anything, so the next
/// `jj spr diff` is what finds the pull request pointing at a base branch whose
/// content has landed, and it must not close it either.
#[test]
fn diff_retargets_a_pull_request_whose_parent_was_merged_on_github() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "diffretarget");

    let tag = run_tag();
    let (bottom_title, top_title) = (
        format!("e2e uimerge bottom {tag}"),
        format!("e2e uimerge top {tag}"),
    );
    let prs = scratch.push_stack(&[&bottom_title, &top_title]);
    let (bottom, top) = (prs[0], prs[1]);

    let old_base = scratch.pr_base_branch(top);
    assert!(
        old_base.starts_with(&scratch.prefix),
        "the top PR should be stacked on a base branch jj-spr made, got {old_base:?}"
    );

    // Merge the bottom the way a reviewer clicking the button would, which
    // leaves the pull request above it stacked on a base branch nobody needs.
    run(
        "gh",
        &[
            "pr",
            "merge",
            &bottom.to_string(),
            "--repo",
            &scratch.repo_arg(),
            "--squash",
            "--delete-branch",
        ],
        scratch.path(),
    );

    // Catch up locally: the change above is now directly on the trunk, which is
    // what tells diff the base branch is obsolete.
    run("jj", &["git", "fetch"], scratch.path());
    run(
        "jj",
        &["rebase", "-r", "@", "-d", "trunk()"],
        scratch.path(),
    );

    jj_spr(&["diff", "-r", "@", "-m", "rebase"], scratch.path());

    assert_eq!(
        scratch.pr_state(top),
        "open",
        "retargeting PR #{top} closed it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.default_branch(),
        "PR #{top} should now target the default branch"
    );
    assert!(
        !scratch.remote_has_branch(&old_base),
        "the base branch PR #{top} left behind is still on the remote: {old_base}"
    );
}

/// Under `spr.baseStrategy = linear` a stacked pull request asks to be merged
/// into the pull request below it, and no base branch is generated at all.
///
/// Only GitHub can show this: what a pull request is based on is a fact about
/// the pull request, and the branches that do or do not exist are a fact about
/// the remote.
#[test]
fn a_linear_stack_bases_each_pull_request_on_the_one_below() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "linear");
    scratch.set_config("spr.baseStrategy", "linear");

    let prs = scratch.push_stack(&["e2e linear bottom", "e2e linear top"]);
    let (bottom, top) = (prs[0], prs[1]);

    assert_eq!(
        scratch.pr_base_branch(bottom),
        scratch.default_branch(),
        "the bottom of a stack is on the default branch under any strategy"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.pr_head_branch(bottom),
        "PR #{top} should be based on the branch of PR #{bottom}"
    );

    let mut expected = vec![scratch.pr_head_branch(bottom), scratch.pr_head_branch(top)];
    expected.sort();
    assert_eq!(
        scratch.remote_branches(),
        expected,
        "the linear strategy should have pushed the two head branches and nothing else"
    );
}

/// Under `spr.stackDisplay = github` the pull requests a run pushes become a stack
/// GitHub itself holds and draws.
///
/// Only GitHub can show this: a stack is a resource on GitHub's side, and
/// whether the pull requests are chained the way it requires is a fact about
/// the pull requests rather than about anything local.
///
/// The target repository must have stacked pull requests enabled. Without it
/// GitHub answers 404 on every stacks route and this fails saying so, which is
/// the honest outcome: the setting was asked for and could not be honoured.
#[test]
fn a_stack_pushed_by_jj_spr_becomes_a_github_stack() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "ghstack");
    // Deliberately the only setting: `spr.stackDisplay = github` requires the linear
    // base strategy and supplies it, and this is where that has to be true of
    // the built binary rather than of a unit test.
    scratch.set_config("spr.stackDisplay", "github");

    let prs = scratch.push_stack(&["e2e ghstack bottom", "e2e ghstack top"]);
    let (bottom, top) = (prs[0], prs[1]);

    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.pr_head_branch(bottom),
        "the setting should have brought the linear strategy with it: PR #{top} should be \
         based on the branch of PR #{bottom}"
    );

    let stack = scratch
        .open_stack_for(bottom)
        .unwrap_or_else(|| panic!("PR #{bottom} should be in an open GitHub stack"));
    assert_eq!(
        stack.pull_requests, prs,
        "the stack should hold the run's pull requests, bottom first"
    );
    assert_eq!(
        scratch.open_stack_for(top).map(|s| s.number),
        Some(stack.number),
        "both pull requests should be in the same stack"
    );
}

/// Amending the bottom of a linear stack moves both branches forward and never
/// rewrites either: jj-spr does not force-push, and GitHub drops the review
/// comments on commits that go missing.
///
/// The pull request above is the one at risk, because it is what has to gain
/// the new commit from below.
#[test]
fn amending_below_a_linear_pull_request_only_moves_branches_forward() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "linearff");
    scratch.set_config("spr.baseStrategy", "linear");

    let prs = scratch.push_stack(&["e2e linearff bottom", "e2e linearff top"]);
    let (bottom, top) = (prs[0], prs[1]);
    let before: Vec<String> = prs.iter().map(|n| scratch.pr_head_sha(*n)).collect();

    // Amend the change at the bottom, which is what the one above is based on.
    run("jj", &["edit", "@-"], scratch.path());
    std::fs::write(
        scratch.path().join(slug("e2e linearff bottom")),
        "amended content",
    )
    .unwrap();
    run("jj", &["edit", "@+"], scratch.path());
    jj_spr(
        &["diff", "--all", "-r", "trunk()..@", "-m", "amend"],
        scratch.path(),
    );

    for (number, before) in prs.iter().zip(&before) {
        let after = scratch.pr_head_sha(*number);
        assert_eq!(
            scratch.compare(before, &after),
            "ahead",
            "PR #{number}'s branch was rewritten rather than moved forward"
        );
    }

    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.pr_head_branch(bottom),
        "PR #{top} should still be based on the branch of PR #{bottom}"
    );
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "pushing below PR #{top} closed it"
    );
}

/// Closing the middle of a linear stack retargets the pull request above it at
/// the closed pull request's own base, and leaves it open.
///
/// Only GitHub can show this. Under `spr.baseStrategy = linear` what the pull
/// request above is based on *is* the closed pull request's head branch, so the
/// branch may only be deleted once that pull request has been moved off it —
/// GitHub closes a pull request whose base branch disappears. Asserting the
/// state as well as the base is the point: were the deletion to run first, the
/// base and the branch would look right while the pull request sat closed.
///
/// The new base is the closed pull request's base rather than the default
/// branch, because closing puts nothing on the default branch: sending the pull
/// request above there would swallow the changes of everything below it, which
/// is still under review.
#[test]
fn closing_below_a_linear_pull_request_retargets_it_and_leaves_it_open() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "closeretarget");
    scratch.set_config("spr.baseStrategy", "linear");

    let tag = run_tag();
    let titles = [
        format!("e2e close bottom {tag}"),
        format!("e2e close middle {tag}"),
        format!("e2e close top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());
    let (bottom, middle, top) = (prs[0], prs[1], prs[2]);

    let bottom_branch = scratch.pr_head_branch(bottom);
    let middle_branch = scratch.pr_head_branch(middle);
    assert_eq!(
        scratch.pr_base_branch(top),
        middle_branch,
        "PR #{top} should be based on the branch of PR #{middle} under the linear strategy"
    );

    // `push_stack` leaves the working copy on the top of the stack, so the
    // middle change is its parent.
    jj_spr(&["close", "-r", "@-"], scratch.path());

    assert_eq!(
        scratch.pr_state(middle),
        "closed",
        "PR #{middle} should have been closed"
    );
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "closing below PR #{top} closed it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        bottom_branch,
        "PR #{top} should have been retargeted at the closed PR's own base"
    );
    assert!(
        !scratch.remote_has_branch(&middle_branch),
        "the closed PR's branch is still on the remote: {middle_branch}"
    );
    assert!(
        scratch.remote_has_branch(&bottom_branch),
        "closing PR #{middle} took away the branch of PR #{bottom} below it: {bottom_branch}"
    );
}

/// Under `spr.baseStrategy = synthetic` the closed pull request's base branch
/// is one jj-spr generated for it alone, so closing does take that one away.
///
/// The counterpart to the linear case above: the rule is ownership, not the
/// strategy, and a repository holds pull requests made under both at once.
#[test]
fn closing_a_synthetic_pull_request_takes_away_its_generated_base_branch() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "closesynth");
    // Set even though it is the default: this is the test whose whole point is
    // the contrast with the linear case above, so it should fail about the
    // strategy rather than about a branch if the default ever changes.
    scratch.set_config("spr.baseStrategy", "synthetic");

    let tag = run_tag();
    let titles = [
        format!("e2e closesynth bottom {tag}"),
        format!("e2e closesynth top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());
    let (bottom, top) = (prs[0], prs[1]);

    let base = scratch.pr_base_branch(top);
    let head = scratch.pr_head_branch(top);
    assert!(
        base.starts_with(&scratch.prefix),
        "the top PR should be stacked on a base branch jj-spr made, got {base:?}"
    );

    // `push_stack` leaves the working copy on the top of the stack.
    jj_spr(&["close", "-r", "@"], scratch.path());

    assert_eq!(
        scratch.pr_state(top),
        "closed",
        "PR #{top} should have been closed"
    );
    assert_eq!(
        scratch.pr_state(bottom),
        "open",
        "closing PR #{top} closed PR #{bottom} below it"
    );
    for branch in [&base, &head] {
        assert!(
            !scratch.remote_has_branch(branch),
            "a branch the closed PR owned is still on the remote: {branch}"
        );
    }
}

/// Adopting `spr.baseStrategy = linear` moves a stacked pull request off the
/// base branch jj-spr generated for it and onto the head branch of the pull
/// request below, and takes the branch it left away.
///
/// This is the arm of the push in `diff` where the pull request ends up on a
/// base branch rather than on the default branch, and it is the only test that
/// reaches the *retarget* inside that arm: every other stacked test either
/// keeps the base it was pushed with or goes back to the default branch. The
/// push, the retarget and the deletion all happen here, in that order — a base
/// branch deleted while the pull request still targets it makes GitHub close
/// it — so the state is asserted alongside the base and the branch's absence,
/// as in the retarget-to-default tests above.
///
/// The change on top is amended because a migration needs something to carry
/// it, which is also what makes the push observable. That is a gap rather than
/// a rule: switching the strategy and nothing else is silently a no-op, because
/// the early return for a change that needs no push comes before the base is
/// looked at.
#[test]
fn migrating_a_stack_to_linear_retargets_it_and_takes_away_its_base_branch() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "linearmigrate");
    // Set even though it is the default: this test is about what changing the
    // strategy does, so it should say which one it starts from.
    scratch.set_config("spr.baseStrategy", "synthetic");

    let top_title = "e2e migrate top";
    let prs = scratch.push_stack(&["e2e migrate bottom", top_title]);
    let (bottom, top) = (prs[0], prs[1]);

    let old_base = scratch.pr_base_branch(top);
    let bottom_branch = scratch.pr_head_branch(bottom);
    assert!(
        old_base.starts_with(&scratch.prefix) && old_base != bottom_branch,
        "the top PR should be stacked on a base branch jj-spr made for it alone, got {old_base:?}"
    );
    let before = scratch.pr_head_sha(top);

    scratch.set_config("spr.baseStrategy", "linear");

    // `push_stack` leaves the working copy on the top of the stack, so this
    // amends the change whose pull request is migrating.
    std::fs::write(scratch.path().join(slug(top_title)), "migrated content").unwrap();
    jj_spr(
        &["diff", "--all", "-r", "trunk()..@", "-m", "migrate"],
        scratch.path(),
    );

    assert_eq!(
        scratch.compare(&before, &scratch.pr_head_sha(top)),
        "ahead",
        "PR #{top}'s branch should have been moved forward by the push"
    );
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "retargeting PR #{top} closed it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        bottom_branch,
        "PR #{top} should now be based on the branch of PR #{bottom}"
    );
    assert!(
        !scratch.remote_has_branch(&old_base),
        "the base branch PR #{top} left behind is still on the remote: {old_base}"
    );
    assert_eq!(
        scratch.pr_state(bottom),
        "open",
        "migrating PR #{top} closed PR #{bottom} below it"
    );
}

/// Retargeting a pull request that GitHub holds in a stack takes it out of that
/// stack first.
///
/// A stack owns its members' base refs: GitHub answers `422 Cannot change the
/// base branch because the pull request is part of a stack` to *any* base sent
/// for a stacked pull request. So a run that moves a base has to dissolve the
/// stack before it sends one, and the pushing path guards every retarget with
/// that.
///
/// Only GitHub can show this. The refusal comes from a resource on GitHub's
/// side that nothing local models, and it is invisible until a stack is really
/// there to hold the pull request — which is why this is the test that stands
/// behind the guard. It is the *only* one: the retarget it makes is to the
/// default branch, and no test sends a base to a stacked pull request by the
/// other route. That is enough only for as long as the guard is asked once,
/// above the two of them; anyone moving it back down into each has taken away
/// the cover for one of them and needs a second test here. This is about
/// `diff`'s two arms only — `land` sends a base as well, and the test below
/// covers its guard.
///
/// The retarget being to the default branch covers that arm of the push as
/// well. The branch the pull request leaves is the head branch of the pull
/// request below, which is not jj-spr's to take away — deleting it would close
/// that pull request — so its survival is asserted too.
#[test]
fn retargeting_a_pull_request_in_a_github_stack_takes_it_out_of_the_stack() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "unstackretarget");
    scratch.set_config("spr.stackDisplay", "github");

    let prs = scratch.push_stack(&["e2e unstack bottom", "e2e unstack top"]);
    let (bottom, top) = (prs[0], prs[1]);
    let bottom_branch = scratch.pr_head_branch(bottom);

    let stack = scratch.open_stack_for(top).unwrap_or_else(|| {
        panic!("PR #{top} should be in an open GitHub stack to be taken out of")
    });
    assert_eq!(
        stack.pull_requests, prs,
        "the stack should hold the run's pull requests, bottom first"
    );

    // Take the change on top out of the stack, so that its pull request belongs
    // on the default branch while GitHub still has it stacked.
    run(
        "jj",
        &["rebase", "-r", "@", "-d", "trunk()"],
        scratch.path(),
    );
    jj_spr(&["diff", "-r", "@", "-m", "off the stack"], scratch.path());

    assert_eq!(
        scratch.pr_state(top),
        "open",
        "retargeting PR #{top} closed it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.default_branch(),
        "PR #{top} should now target the default branch"
    );
    assert_eq!(
        scratch.open_stack_for(top).map(|s| s.number),
        None,
        "PR #{top} should have been taken out of stack #{}",
        stack.number
    );
    assert!(
        scratch.remote_has_branch(&bottom_branch),
        "the branch of PR #{bottom} below is not this one's to take away: {bottom_branch}"
    );
    assert_eq!(
        scratch.pr_state(bottom),
        "open",
        "retargeting PR #{top} closed PR #{bottom} below it"
    );
}

/// Landing a pull request a GitHub stack holds takes the stack apart first and
/// merges that pull request on its own, leaving the ones above it open, still
/// carrying their own changes, and pointed at the default branch.
///
/// Only GitHub can show this, and only against a stack GitHub is holding. This
/// test is mostly about what did *not* happen: the alternative — merging
/// through the stack, which is what GitHub's API is for — passes every local
/// test and then destroys the pull request above, for the reasons set out at
/// the top of `impl GitHub` in `github::stacks`. A stack of three, landing the
/// middle, is the smallest
/// shape that has both a pull request the stack merge would have dragged in and
/// one it would have destroyed.
#[test]
fn landing_a_pull_request_in_a_github_stack_leaves_the_rest_of_the_stack_alone() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "ghstackland");
    scratch.set_config("spr.stackDisplay", "github");

    // The tag keeps this run's commits off every earlier run's: what this test
    // merges stays on the default branch, and a change that adds a file that is
    // already there with the same content is empty.
    let tag = run_tag();
    let titles = [
        format!("e2e ghstack land bottom {tag}"),
        format!("e2e ghstack land middle {tag}"),
        format!("e2e ghstack land top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());
    let (bottom, middle, top) = (prs[0], prs[1], prs[2]);

    let stack = scratch
        .open_stack_for(middle)
        .unwrap_or_else(|| panic!("PR #{middle} should be in an open GitHub stack to be landed"));
    assert_eq!(
        stack.pull_requests, prs,
        "the stack should hold the run's pull requests, bottom first"
    );

    let middle_branch = scratch.pr_head_branch(middle);
    assert_eq!(
        scratch.pr_base_branch(top),
        middle_branch,
        "PR #{top} should be based on the branch of PR #{middle} for this to be a stack"
    );

    // The change below `@` is the middle one: `push_stack` leaves `@` on top.
    let landed = jj_spr(&["land", "-r", "@-"], scratch.path());
    let said = landed.split_whitespace().collect::<Vec<_>>().join(" ");

    // Taking a stack apart is not undoable and does not undo itself, so it is
    // said as it happens.
    assert!(
        said.contains(&format!("Dissolved GitHub stack #{}", stack.number)),
        "landing PR #{middle} had to say it was taking stack #{} apart:\n{landed}",
        stack.number
    );

    assert_eq!(
        scratch.pr_field(middle, ".merged"),
        "true",
        "PR #{middle} should have been merged"
    );

    // The whole point: only the pull request that was asked for.
    assert_eq!(
        scratch.pr_state(bottom),
        "open",
        "landing PR #{middle} must not close PR #{bottom} below it"
    );
    assert_eq!(
        scratch.pr_field(bottom, ".merged"),
        "false",
        "landing PR #{middle} must not merge PR #{bottom} below it"
    );
    // Its *content* does land with PR #{middle} — under the linear base
    // strategy that pull request's head carries the whole stack's tree, so the
    // squash commit on the default branch holds both changes. GitHub still
    // reports PR #{bottom} as changing its own file, because it compares
    // against the merge base rather than against the branch tip. So the
    // difference from GitHub's stack merge is not that nothing below lands: it
    // is that the pull requests around this one stay open, keep their branches,
    // and keep their reviews.

    assert_eq!(
        scratch.pr_state(top),
        "open",
        "landing below PR #{top} closed it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.default_branch(),
        "PR #{top} should now target the default branch"
    );
    // The killer symptom of the stack merge: the survivor's branch reset onto
    // its base, so its diff is empty and its review is gone.
    assert_ne!(
        scratch.pr_field(top, ".changed_files"),
        "0",
        "PR #{top} still has to carry its own change"
    );

    assert_eq!(
        scratch.open_stack_for(top).map(|s| s.number),
        None,
        "stack #{} should have been dissolved, not left holding a merged pull request",
        stack.number
    );

    assert!(
        !scratch.remote_has_branch(&middle_branch),
        "the base branch PR #{top} left behind is still on the remote: {middle_branch}"
    );
}

/// Closing a pull request a GitHub stack holds takes the stack apart first, so
/// that the pull requests above it can be pointed at the closed one's own base
/// and stay open.
///
/// Only GitHub can show this. Closing is a state change and GitHub allows it
/// while a stack holds the pull request — the stack simply keeps the closed
/// member in place — but the base changes that follow are refused with `422
/// Cannot change the base branch because the pull request is part of a stack`,
/// and that refusal exists nowhere but on GitHub's side. Without the dissolve
/// the close "succeeds", the pull request above is left pointing at the closed
/// one, and its branch is kept because nothing could be moved off it: exactly
/// what the base and branch assertions here catch.
///
/// A stack of three, closing the middle, is the smallest shape with both a pull
/// request below that must be left alone and one above that must be moved onto
/// it.
#[test]
fn closing_a_pull_request_in_a_github_stack_retargets_the_rest_and_leaves_them_open() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "ghstackclose");
    scratch.set_config("spr.stackDisplay", "github");

    let prs = scratch.push_stack(&[
        "e2e ghstack close bottom",
        "e2e ghstack close middle",
        "e2e ghstack close top",
    ]);
    let (bottom, middle, top) = (prs[0], prs[1], prs[2]);

    let stack = scratch
        .open_stack_for(middle)
        .unwrap_or_else(|| panic!("PR #{middle} should be in an open GitHub stack to be closed"));
    assert_eq!(
        stack.pull_requests, prs,
        "the stack should hold the run's pull requests, bottom first"
    );

    let bottom_branch = scratch.pr_head_branch(bottom);
    let middle_branch = scratch.pr_head_branch(middle);
    assert_eq!(
        scratch.pr_base_branch(top),
        middle_branch,
        "PR #{top} should be based on the branch of PR #{middle} for this to be a stack"
    );

    // The change below `@` is the middle one: `push_stack` leaves `@` on top.
    let closed = jj_spr(&["close", "-r", "@-"], scratch.path());
    let said = closed.split_whitespace().collect::<Vec<_>>().join(" ");

    // Taking a stack apart is not undoable and does not undo itself, so it is
    // said as it happens.
    assert!(
        said.contains(&format!("Dissolved GitHub stack #{}", stack.number)),
        "closing PR #{middle} had to say it was taking stack #{} apart:\n{closed}",
        stack.number
    );
    // ...and the stack does not come back: only `jj spr diff` registers one, and
    // it gets a new number when it does. So the pull requests the dissolve freed
    // have to be named — all of them except the one just closed, which nothing
    // can put back into a stack and which needs no putting back.
    assert!(
        said.contains(&format!("GitHub stack: #{bottom}, #{top} left unstacked")),
        "exactly the freed pull requests that are still open, and not the closed PR #{middle}, \
         should be reported as left unstacked:\n{closed}"
    );

    assert_eq!(
        scratch.pr_state(middle),
        "closed",
        "PR #{middle} should have been closed"
    );

    // The whole point: only the pull request that was asked for.
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "closing PR #{middle} closed PR #{top} above it"
    );
    assert_eq!(
        scratch.pr_state(bottom),
        "open",
        "closing PR #{middle} closed PR #{bottom} below it"
    );

    // Onto the closed pull request's own base, not the default branch: closing
    // puts nothing there, so PR #{top} would swallow PR #{bottom}'s changes.
    assert_eq!(
        scratch.pr_base_branch(top),
        bottom_branch,
        "PR #{top} should have been retargeted at the closed PR's own base"
    );
    assert!(
        !scratch.remote_has_branch(&middle_branch),
        "the closed PR's branch is still on the remote, so PR #{top} was never moved off it: \
         {middle_branch}"
    );
    assert!(
        scratch.remote_has_branch(&bottom_branch),
        "closing PR #{middle} took away the branch of PR #{bottom} below it: {bottom_branch}"
    );

    assert_eq!(
        scratch.open_stack_for(top).map(|s| s.number),
        None,
        "stack #{} should have been dissolved to let PR #{top} move",
        stack.number
    );
}

/// Squash-landing a whole GitHub native stack, bottom first, puts exactly one
/// commit per pull request on the default branch, each carrying that pull
/// request's own change and nothing else.
///
/// This is the property that makes `squash` the merge method jj-spr defaults
/// to, and only GitHub can show it. Every pull request in a native stack is
/// based on the head branch of the one below, so its branch carries the whole
/// stack's tree: what the *diff* of a landed pull request comes to is worked out
/// by GitHub, from a merge base that moves as each land goes in, and nothing
/// local models that. Landing the middle on its own already puts two changes in
/// one commit — the test above says so — so the collapse asserted here is a
/// property of landing the stack *in order*, not of any one land.
///
/// Nothing is done to the local repository between the lands. `jj spr land`
/// leaves the working copy where it was — it says so, "Please manually rebase
/// your working copy after landing" — and that is fine here, because nothing
/// the next land needs comes from the working copy being current: `land` finds
/// its pull request by the number recorded in the local commit's message, which
/// no land rewrites, and the retarget onto the default branch that lets the next
/// one merge has already been done on GitHub by the land below it. A `jj git
/// fetch` and a `jj rebase` would be what a *person* wants next, to keep working
/// on the rest of the stack; they are not what makes the next land work, so
/// putting them here would only hide whether it does.
#[test]
fn squash_landing_a_github_stack_lands_one_commit_per_pull_request() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "ghstacksquash");
    scratch.set_config("spr.stackDisplay", "github");

    // The tag keeps this run's commits off every earlier run's: what this test
    // merges stays on the default branch, and a change that adds a file that is
    // already there with the same content is empty.
    let tag = run_tag();
    let titles = [
        format!("e2e squashland bottom {tag}"),
        format!("e2e squashland middle {tag}"),
        format!("e2e squashland top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());

    let stack = scratch
        .open_stack_for(prs[0])
        .unwrap_or_else(|| panic!("PR #{} should be in an open GitHub stack", prs[0]));
    assert_eq!(
        stack.pull_requests, prs,
        "the stack should hold the run's pull requests, bottom first"
    );

    let before = scratch.default_branch_sha();

    // `push_stack` leaves `@` on the top of the stack, so the three changes are
    // `@--`, `@-` and `@`, bottom to top.
    for revision in ["@--", "@-", "@"] {
        jj_spr(&["land", "-r", revision], scratch.path());
    }

    for number in &prs {
        assert_eq!(
            scratch.pr_field(*number, ".merged"),
            "true",
            "PR #{number} should have been merged"
        );
    }

    let landed = scratch.commits_landed_since(&before);
    assert_eq!(
        landed.len(),
        titles.len(),
        "landing a stack of {} should put one commit on the default branch per pull request, \
         got {landed:?}",
        titles.len()
    );

    // Oldest first, which is the order they were landed in.
    for (sha, title) in landed.iter().zip(&titles) {
        assert_eq!(
            scratch.commit_files(sha),
            vec![slug(title)],
            "the commit {sha} that landed for {title:?} should carry that change and no other"
        );
    }
}

/// Landing the bottom of a GitHub native stack needs no `--force`.
///
/// Only GitHub can show this, and it is the one land whose verdict is taken
/// while a stack is still holding the pull request: the bottom of a stack is
/// already based on the default branch, so `land` asks GitHub whether it will
/// merge *before* dissolving anything, and that is the only order in which
/// GitHub is ever asked about a stacked pull request. If GitHub answered
/// `BLOCKED` for a pull request merely because a stack holds it, then
/// `merge_requirements` would read that as unmet and, with the default
/// `spr.landWithUnmetRequirements = false`, every land of the bottom of a stack
/// would be refused — naming a failing check or a missing review as the cause,
/// when the real cause was the stack. So this lands without `--force` on
/// purpose: passing it would skip the very check being pinned.
///
/// A stack of three, landing the bottom, is the smallest shape where the pull
/// request being landed is stacked and still based on the default branch.
#[test]
fn landing_the_bottom_of_a_github_stack_needs_no_force() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "ghstackbottom");
    scratch.set_config("spr.stackDisplay", "github");

    // The tag keeps this run's commits off every earlier run's: what this test
    // merges stays on the default branch, and a change that adds a file that is
    // already there with the same content is empty.
    let tag = run_tag();
    let titles = [
        format!("e2e bottomland bottom {tag}"),
        format!("e2e bottomland middle {tag}"),
        format!("e2e bottomland top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());
    let (bottom, middle) = (prs[0], prs[1]);

    let stack = scratch
        .open_stack_for(bottom)
        .unwrap_or_else(|| panic!("PR #{bottom} should be in an open GitHub stack to be landed"));
    assert_eq!(
        stack.pull_requests, prs,
        "the stack should hold the run's pull requests, bottom first"
    );
    assert_eq!(
        scratch.pr_base_branch(bottom),
        scratch.default_branch(),
        "the bottom of a stack is on the default branch, which is what makes this the land \
         that asks GitHub while the stack still holds the pull request"
    );

    // Read while the stack is still holding it, because that is the state in
    // question: a refusal below is only interpretable next to what GitHub was
    // reporting for a stacked pull request at the moment it was asked.
    let stacked_verdict = scratch.pr_merge_state(bottom);

    // `push_stack` leaves `@` on the top of the stack, so the bottom is `@--`.
    let landed = try_jj_spr(&["land", "-r", "@--"], scratch.path()).unwrap_or_else(|said| {
        panic!(
            "landing the bottom of stack #{} without --force was refused. GitHub reported \
             {stacked_verdict} for PR #{bottom} while the stack held it, and jj-spr said:\n{said}",
            stack.number
        )
    });
    assert!(
        landed.contains(&format!("Dissolved GitHub stack #{}", stack.number)),
        "landing PR #{bottom} had to say it was taking stack #{} apart:\n{landed}",
        stack.number
    );

    assert_eq!(
        scratch.pr_field(bottom, ".merged"),
        "true",
        "PR #{bottom} should have been merged"
    );
    assert_eq!(
        scratch.pr_state(middle),
        "open",
        "landing below PR #{middle} closed it"
    );
}
