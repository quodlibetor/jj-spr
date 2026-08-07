/*
 * End-to-end tests against a real GitHub repository.
 *
 * These are the only tests that exercise the whole path a stacked PR takes:
 * pushing branches, opening pull requests, and reading their bodies back off
 * GitHub. Everything else in the suite stops at the library boundary.
 *
 * What belongs here, now that `fake_github_diff_test.rs` can run `diff` against a
 * fake GitHub and a bare repository without a network: everything that is a fact
 * about *GitHub*. What a retarget does to a pull request, what a deleted base
 * branch does to one, what its stack merge does to the ones above, what it makes
 * of a force-pushed branch, what its stacks API accepts. A test against a fake
 * cannot establish any of that — it would only be asserting the fake — so these
 * are also the tests that keep that fake honest, and several of them say below
 * which of its rules they pin. A test whose subject is what jj-spr decides
 * belongs over there instead, where it costs seconds rather than a minute.
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

    /// The commits `head` carries on top of `base` on the remote, oldest first,
    /// each as `<number of parents> <first line of the message>`.
    ///
    /// Both halves are what the rebasing strategy is about. A `2` is a merge
    /// commit, which is the shape GitHub's stack merge destroys — so a strategy
    /// that promises to avoid them can only be checked against the remote,
    /// because the promise is about what GitHub is given. The messages are how a
    /// replay is told from a branch that started again from its base: the
    /// commits come back with new ids either way, and only their messages say
    /// whether the review history survived.
    fn commits_ahead(&self, base: &str, head: &str) -> Vec<String> {
        let path = format!(
            "repos/{}/{}/compare/{base}...{head}",
            self.target.owner, self.target.repo
        );

        run(
            "gh",
            &[
                "api",
                &path,
                "--jq",
                r#".commits[] | "\(.parents | length) \(.commit.message | split("\n")[0])""#,
            ],
            self.path(),
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// The files pull request `number` shows as changed, sorted.
    ///
    /// GitHub works these out against the merge base of the two branches, which
    /// is why a stacked pull request whose branch has lost sight of the branch
    /// below it shows that change's files here as well as its own.
    fn pr_files(&self, number: u64) -> Vec<String> {
        let path = format!(
            "repos/{}/{}/pulls/{number}/files",
            self.target.owner, self.target.repo
        );

        let mut files: Vec<String> = run(
            "gh",
            &["api", "--paginate", &path, "--jq", ".[].filename"],
            self.path(),
        )
        .lines()
        .map(str::to_owned)
        .collect();

        files.sort();
        files
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

    /// What `branch` points at on the remote right now.
    ///
    /// Asked of the remote rather than of the pull request that has the branch
    /// as its head, because the two do not agree straight after a push: GitHub
    /// updates a pull request's `head.sha` lazily, and a test that reads it
    /// immediately gets the value from before the push perhaps one run in
    /// three. Observed against the live API on 2026-08-06, `head.sha`
    /// unchanged while `ls-remote` already had the new commit. The branch is
    /// the thing under test wherever this is used, so there is nothing to be
    /// gained by asking the slower of the two.
    fn remote_branch_sha(&self, branch: &str) -> String {
        let listing = run(
            "git",
            &["ls-remote", "--heads", "origin", branch],
            self.path(),
        );

        listing
            .split_whitespace()
            .next()
            .unwrap_or_else(|| panic!("branch {branch} is not on the remote"))
            .to_owned()
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
///
/// This is what pins the chain rule for the fake in `fake_github_diff_test.rs` —
/// that GitHub accepts as a stack exactly the pull requests whose base ref is the
/// head ref of the one below — and, since it sets one setting and lets the binary
/// resolve the rest, the only test that covers `main.rs` choosing a base strategy
/// from `spr.stackDisplay = github`.
#[test]
fn a_stack_pushed_by_jj_spr_becomes_a_github_stack() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "ghstack");
    // Deliberately the only setting: `spr.stackDisplay = github` requires a linear base
    // strategy and supplies `linear-rebase`, and this is where that has to be
    // true of the built binary rather than of a unit test.
    scratch.set_config("spr.stackDisplay", "github");

    let prs = scratch.push_stack(&["e2e ghstack bottom", "e2e ghstack top"]);
    let (bottom, top) = (prs[0], prs[1]);

    let bottom_branch = scratch.pr_head_branch(bottom);
    assert_eq!(
        scratch.pr_base_branch(top),
        bottom_branch,
        "the setting should have brought a linear strategy with it: PR #{top} should be \
         based on the branch of PR #{bottom}"
    );
    // Which of the two linear strategies it supplied, asserted where it counts:
    // GitHub's stack merge rebases the branch above the pull request it merges,
    // and a merge commit does not survive that.
    assert_eq!(
        scratch.commits_ahead(&bottom_branch, &scratch.pr_head_branch(top)),
        vec!["1 e2e ghstack top".to_string()],
        "the setting should have supplied the strategy whose branches GitHub can rebase"
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

/// Amending the bottom of a `linear-rebase` stack replays the commits of the
/// pull request above onto the new tip of the branch below — rewriting them,
/// which is what this strategy trades away, while keeping their number and their
/// messages, which is what it keeps.
///
/// The pull request above is what is at risk, and its `Files changed` is what
/// says whether the replay worked: GitHub diffs a pull request from the merge
/// base of the two branches, so a branch left behind on the old tip of the
/// branch below would show that change's file here as well as its own — the
/// change below would be under review twice, in two pull requests.
///
/// The local twin of this test, in `fake_github_diff_test.rs`, asserts the same
/// branches without a network. What only this one can say is what GitHub makes of
/// them: that a force-pushed branch leaves its pull request open, and that
/// `Files changed` is recomputed from the new merge base rather than from
/// wherever the branch used to sit. Both are assumptions the fake bakes in.
#[test]
fn amending_below_a_linear_rebase_pull_request_replays_its_commits() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "rebasereplay");
    scratch.set_config("spr.baseStrategy", "linear-rebase");

    let (bottom_title, top_title) = ("e2e replay bottom", "e2e replay top");
    let prs = scratch.push_stack(&[bottom_title, top_title]);
    let (bottom, top) = (prs[0], prs[1]);
    let (bottom_branch, top_branch) = (scratch.pr_head_branch(bottom), scratch.pr_head_branch(top));

    // Give the pull request above some review history to lose: a second round
    // of the change, which is a second commit on its branch. `push_stack` leaves
    // the working copy on the top change.
    std::fs::write(scratch.path().join(slug(top_title)), "a second round").unwrap();
    jj_spr(
        &["diff", "-r", "@", "-m", "the second round"],
        scratch.path(),
    );
    assert_eq!(
        scratch.commits_ahead(&bottom_branch, &top_branch),
        vec![
            "1 e2e replay top".to_string(),
            "1 the second round".to_string()
        ],
        "the amend should have added a second commit to PR #{top}'s branch"
    );

    let before_top = scratch.remote_branch_sha(&top_branch);
    let before_bottom = scratch.remote_branch_sha(&bottom_branch);

    // Amend the change at the bottom, which is what the one above is based on.
    run("jj", &["edit", "@-"], scratch.path());
    std::fs::write(
        scratch.path().join(slug(bottom_title)),
        "amended bottom content",
    )
    .unwrap();
    run("jj", &["edit", "@+"], scratch.path());
    jj_spr(
        &["diff", "--all", "-r", "trunk()..@", "-m", "amend"],
        scratch.path(),
    );

    let after_bottom = scratch.remote_branch_sha(&bottom_branch);
    assert_eq!(
        scratch.compare(&before_bottom, &after_bottom),
        "ahead",
        "nothing moved under PR #{bottom}, so its own branch should have moved forward \
         ({bottom_branch}: {before_bottom} -> {after_bottom})"
    );

    let after_top = scratch.remote_branch_sha(&top_branch);
    let moved = scratch.compare(&before_top, &after_top);
    assert!(
        matches!(moved.as_str(), "diverged" | "behind"),
        "PR #{top}'s branch should have been rewritten rather than added to, which is \
         what this strategy gives up ({top_branch}: {before_top} -> {after_top} is {moved})"
    );

    assert_eq!(
        scratch.commits_ahead(&bottom_branch, &top_branch),
        vec![
            "1 e2e replay top".to_string(),
            "1 the second round".to_string()
        ],
        "both rounds of PR #{top} should have come along, still one parent each"
    );
    assert_eq!(
        scratch.pr_files(top),
        vec![slug(top_title)],
        "PR #{top} should still be reviewing its own change and nothing else"
    );

    assert_eq!(
        scratch.pr_base_branch(top),
        bottom_branch,
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
///
/// It also pins the rule `base_branch_to_take_away` states and the fake in
/// `fake_github_diff_test.rs` applies: that a base branch a pull request has
/// been moved off can be deleted without closing it, and that the order is what
/// makes that true.
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

/// `jj spr land --stack` hands the whole chain to GitHub: one request merges the
/// pull request and the one below it, one squash commit each, and GitHub moves
/// the pull request above onto the default branch and rebases its branch.
///
/// Only GitHub can show any of it — the merge, the retarget and the rebase are
/// all its work — and this is the test that says the endpoint may be called at
/// all. Under `spr.baseStrategy = linear-rebase` the branch it rebases survives,
/// which is asserted here as the pull request above still being open and still
/// showing only its own change. Under the merging strategies it does not, which
/// is why the land below this one refuses to ask.
///
/// The stack is left standing, holding all three, so nothing has to be
/// registered again afterwards — the one thing a land through jj-spr's own merge
/// cannot manage.
///
/// A stack of three, landing the middle, is the smallest shape with a pull
/// request below to be merged along with it and one above to be left standing.
#[test]
fn landing_through_the_stack_merge_lands_the_chain_and_rebases_the_rest() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "stackmerge");
    scratch.set_config("spr.stackDisplay", "github");

    // What this test merges stays on the default branch, so the tag keeps its
    // changes off every earlier run's.
    let tag = run_tag();
    let titles = [
        format!("e2e stackmerge bottom {tag}"),
        format!("e2e stackmerge middle {tag}"),
        format!("e2e stackmerge top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());
    let (bottom, middle, top) = (prs[0], prs[1], prs[2]);

    let stack = scratch
        .open_stack_for(bottom)
        .unwrap_or_else(|| panic!("PR #{bottom} should be in an open GitHub stack"));
    let branches: Vec<String> = prs.iter().map(|n| scratch.pr_head_branch(*n)).collect();
    let before = scratch.default_branch_sha();
    let top_head_before = scratch.remote_branch_sha(&branches[2]);

    // `push_stack` leaves `@` on the top of the stack, so the middle change is
    // its parent.
    jj_spr(&["land", "--stack", "-r", "@-"], scratch.path());

    for number in [bottom, middle] {
        assert_eq!(
            scratch.pr_field(number, ".merged"),
            "true",
            "PR #{number} should have been merged by the stack merge"
        );
    }

    let landed = scratch.commits_landed_since(&before);
    assert_eq!(
        landed.len(),
        2,
        "the stack merge should put one commit on the default branch per pull request it \
         merged, got {landed:?}"
    );
    for (sha, title) in landed.iter().zip(&titles) {
        assert_eq!(
            scratch.commit_files(sha),
            vec![slug(title)],
            "the commit {sha} that landed for {title:?} should carry that change and no other"
        );
    }

    // The whole reason this endpoint is usable at all: the pull request above
    // survives the rebase GitHub gives its branch.
    assert_eq!(
        scratch.pr_state(top),
        "open",
        "the stack merge closed PR #{top} above it"
    );
    assert_eq!(
        scratch.pr_base_branch(top),
        scratch.default_branch(),
        "GitHub should have moved PR #{top} onto the default branch"
    );
    assert_eq!(
        scratch.pr_files(top),
        vec![slug(&titles[2])],
        "PR #{top} should be reviewing its own change and nothing else"
    );
    assert_eq!(
        scratch.commits_ahead(&scratch.default_branch(), &branches[2]),
        vec![format!("1 {}", titles[2])],
        "PR #{top}'s branch should be its own commit on the new default branch"
    );

    let top_head_after = scratch.remote_branch_sha(&branches[2]);
    assert_ne!(
        top_head_before, top_head_after,
        "GitHub should have rebased PR #{top}'s branch onto what landed"
    );

    // The branches of what merged are jj-spr's to take away, and the stack is
    // nobody's to take apart.
    for (number, branch) in [(bottom, &branches[0]), (middle, &branches[1])] {
        assert!(
            !scratch.remote_has_branch(branch),
            "the branch of merged PR #{number} is still on the remote: {branch}"
        );
    }
    assert_eq!(
        scratch.open_stack_for(top).map(|s| s.number),
        Some(stack.number),
        "the stack merge should have left the stack standing, still holding PR #{top}"
    );
}

/// `jj spr land --stack` is refused under `spr.baseStrategy = linear`, where the
/// branches GitHub would rebase are merge commits that do not survive it.
///
/// The refusal is the whole feature working: this is the land that would destroy
/// the pull request above. Nothing is merged, so the assertion is that both pull
/// requests are still open and the default branch has not moved.
#[test]
fn the_stack_merge_is_refused_where_the_branches_would_not_survive_it() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "stackmergerefused");
    scratch.set_config("spr.stackDisplay", "github");
    scratch.set_config("spr.baseStrategy", "linear");

    let prs = scratch.push_stack(&["e2e refused bottom", "e2e refused top"]);
    let (bottom, top) = (prs[0], prs[1]);
    let before = scratch.default_branch_sha();

    let refusal = try_jj_spr(&["land", "--stack", "-r", "@-"], scratch.path())
        .expect_err("--stack should be refused under spr.baseStrategy = linear");

    assert!(
        refusal.contains("linear-rebase"),
        "the refusal should name the strategy that makes the stack merge safe: {refusal}"
    );
    for number in [bottom, top] {
        assert_eq!(
            scratch.pr_state(number),
            "open",
            "a refused land should have merged nothing, but PR #{number} is not open"
        );
    }
    assert_eq!(
        scratch.default_branch_sha(),
        before,
        "a refused land should have put nothing on the default branch"
    );
}

/// Landing a pull request a GitHub stack holds takes the stack apart first,
/// merges it and the ones below it, and leaves the ones above open, still
/// carrying their own changes, and pointed at the default branch.
///
/// Only GitHub can show this, and only against a stack GitHub is holding. This
/// test is mostly about what did *not* happen: the alternative — merging
/// through the stack, which is what GitHub's API is for — passes every local
/// test and then destroys the pull request above, for the reasons set out at
/// the top of `impl GitHub` in `github::stacks`.
///
/// So the difference from GitHub's stack merge was never that nothing below
/// lands. Both land everything below, and that is right. The difference is
/// entirely above: jj-spr leaves those pull requests open, with their branches,
/// their diffs and their reviews, where the stack merge rebases the next one
/// onto its new base and collapses it.
///
/// A stack of three, landing the middle, is the smallest shape that has both a
/// pull request below to be landed with it and one above to be left standing.
#[test]
fn landing_a_pull_request_in_a_github_stack_leaves_the_ones_above_it_alone() {
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

    // The pull request below is landed in its own right rather than having its
    // content dragged along inside this one's squash. Under the linear base
    // strategy PR #{middle}'s head carries the whole stack's tree, so its
    // changes were going to reach the default branch either way; merging it as
    // its own pull request is what closes it, deletes its branch, and puts its
    // change on the default branch under its own title.
    assert_eq!(
        scratch.pr_field(bottom, ".merged"),
        "true",
        "landing PR #{middle} had to land PR #{bottom} below it, whose change its branch carries"
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

/// Landing the top of a stack lands everything under it, one commit each.
///
/// The behaviour this pins is the whole of why `land` walks a chain rather than
/// merging the one pull request it was pointed at. A pull request branch
/// carries the local stack below it, so merging the top one alone would put all
/// three changes on the default branch inside a single squash under the top
/// one's title, and leave the two pull requests below open with nothing left to
/// show. Only GitHub can tell the two apart: what is being read back is the
/// shape of the default branch after the merges, and which pull requests GitHub
/// itself closed as merged.
///
/// A stack of three, landing the top in one command, is the smallest shape
/// where a cascade is more than one merge and the order of the merges matters.
/// It runs without `spr.stackDisplay = github`, because nothing here is about GitHub's
/// stacks — a stack of jj-spr's own is enough to have unlanded parents.
#[test]
fn landing_the_top_of_a_stack_lands_the_pull_requests_below_it_first() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "cascadeland");

    // The tag keeps this run's commits off every earlier run's: what this test
    // merges stays on the default branch, and a change that adds a file that is
    // already there with the same content is empty.
    let tag = run_tag();
    let titles = [
        format!("e2e cascadeland bottom {tag}"),
        format!("e2e cascadeland middle {tag}"),
        format!("e2e cascadeland top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());

    let before = scratch.default_branch_sha();

    // `push_stack` leaves `@` on the top of the stack, so this is the one land
    // in question: the pull request named is the only one asked for, and the
    // two below it are landed because `land` works out that it has to.
    let landed = jj_spr(&["land", "-r", "@"], scratch.path());
    assert!(
        landed.contains(&format!(
            "Landing 3 Pull Requests, bottom first: #{}, #{}, #{}",
            prs[0], prs[1], prs[2]
        )),
        "landing PR #{} had to say it was landing the two below it first:\n{landed}",
        prs[2]
    );

    for number in &prs {
        assert_eq!(
            scratch.pr_field(*number, ".merged"),
            "true",
            "PR #{number} should have been merged by a land asked only for #{}",
            prs[2]
        );
    }

    let landed_commits = scratch.commits_landed_since(&before);
    assert_eq!(
        landed_commits.len(),
        titles.len(),
        "landing the top of a stack of {} should put one commit on the default branch per pull \
         request, not one squash carrying all of them, got {landed_commits:?}",
        titles.len()
    );

    // Oldest first, which is the order they were landed in: bottom to top.
    for (sha, title) in landed_commits.iter().zip(&titles) {
        assert_eq!(
            scratch.commit_files(sha),
            vec![slug(title)],
            "the commit {sha} that landed for {title:?} should carry that change and no other"
        );
    }
}

/// A change with no pull request below the one being landed refuses the land.
///
/// The refusal is not fussiness about tidy state: the unpushed change's commits
/// are in the branch of the pull request above it, so a land that passed over it
/// would put it on the default branch anyway, with no pull request to record
/// that it went. Refusing is the only outcome that does not land something
/// silently.
///
/// Pinned end to end rather than in a unit test because the fact being checked
/// is about the local chain jj-spr reads back from Jujutsu, and the same shape
/// is what a half-pushed stack looks like in practice.
#[test]
fn landing_over_a_change_with_no_pull_request_is_refused() {
    let Some(target) = target() else {
        eprintln!("skipping: set E2E_TEST_REPO to run");
        return;
    };
    let scratch = Scratch::new(target, "cascadegap");

    let tag = run_tag();
    let titles = [
        format!("e2e cascadegap bottom {tag}"),
        format!("e2e cascadegap top {tag}"),
    ];
    let prs = scratch.push_stack(&titles.iter().map(String::as_str).collect::<Vec<_>>());

    // Slide an unpushed change in between the two, which is what a stack looks
    // like when only part of it has been through `jj spr diff`. `@` stays on
    // the top change, which is the one being landed.
    let gap = format!("e2e cascadegap unpushed {tag}");
    run(
        "jj",
        &["new", "-A", "@-", "-m", &describe(&gap)],
        scratch.path(),
    );
    std::fs::write(scratch.path().join(slug(&gap)), &gap).unwrap();
    run("jj", &["edit", "@+"], scratch.path());

    let before = scratch.default_branch_sha();

    let said = try_jj_spr(&["land", "-r", "@"], scratch.path()).expect_err(
        "landing over a change with no pull request should be refused, not land the change",
    );
    // jj-spr wraps what it says to the terminal, so the sentence is matched
    // with its own line breaks taken out rather than in fragments short enough
    // to survive them.
    let unwrapped = said.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        unwrapped.contains("is below the one being landed and has no Pull Request"),
        "the refusal had to name the missing pull request as the reason:\n{said}"
    );
    assert!(
        unwrapped.contains(&gap),
        "the refusal had to name which change is missing a pull request:\n{said}"
    );

    assert_eq!(
        scratch.commits_landed_since(&before),
        Vec::<String>::new(),
        "a refused land must not have put anything on the default branch"
    );
    for number in &prs {
        assert_eq!(
            scratch.pr_state(*number),
            "open",
            "PR #{number} should still be open after a refused land"
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
