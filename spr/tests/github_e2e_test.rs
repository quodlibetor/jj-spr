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
            let branches: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| Some(line.split_whitespace().nth(1)?.to_owned()))
                .collect();
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
