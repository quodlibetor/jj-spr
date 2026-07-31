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

/// A clone of the target repository, configured for spr, that closes whatever
/// it opened when it goes out of scope.
struct Scratch {
    target: Target,
    dir: tempfile::TempDir,
    prefix: String,
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
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
