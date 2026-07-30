/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashSet;

use crate::{
    error::{Error, Result},
    github::GitHubBranch,
    utils::slugify,
};

/// Which branch a stacked pull request asks to be merged into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseStrategy {
    /// Give every stacked pull request a base branch of its own, carrying the
    /// tree of the local parent change.
    ///
    /// Each pull request is then self-contained: the base branch is built from
    /// the local parent, so a change can be pushed without its parent being up
    /// to date on GitHub.
    #[default]
    Synthetic,
    /// Base a stacked pull request on the pull request branch of the change
    /// below it, so that the stack on GitHub is a chain of branches.
    ///
    /// The pull requests below have to be pushed first — which `diff` does
    /// anyway when it is given the whole stack — because a stale parent branch
    /// would leak the parent's changes into this pull request's diff.
    Linear,
}

impl std::str::FromStr for BaseStrategy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "synthetic" => Ok(Self::Synthetic),
            "linear" => Ok(Self::Linear),
            other => Err(Error::new(format!(
                "spr.baseStrategy must be 'synthetic' or 'linear', but is '{other}'"
            ))),
        }
    }
}

impl BaseStrategy {
    /// Every strategy, in the order `jj spr init` offers them: the default
    /// first, then in increasing order of what a stack asks of GitHub.
    ///
    /// Kept next to the enum rather than in `init`, so that a strategy added
    /// here is offered rather than quietly left out of the one place that asks
    /// about it.
    pub const ALL: [Self; 2] = [Self::Synthetic, Self::Linear];

    /// The value `spr.baseStrategy` takes for this strategy.
    ///
    /// The inverse of the [`FromStr`](std::str::FromStr) above, and here rather
    /// than spelled out wherever a strategy is written: `jj spr init` offers
    /// these names and then stores the one that was picked, so the two
    /// directions have to agree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic",
            Self::Linear => "linear",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub owner: String,
    pub repo: String,
    pub remote_name: String,
    pub master_ref: GitHubBranch,
    pub branch_prefix: String,
    pub require_approval: bool,
    /// Land a pull request even when GitHub reports the requirements its base
    /// branch sets as unmet.
    ///
    /// Not a parameter of [`Config::new`]: it sits next to `require_approval`,
    /// and a second bool in that position is one transposed argument away from
    /// silently disabling the check it guards. Set it with struct update
    /// syntax instead, which names the field at the call site.
    pub land_with_unmet_requirements: bool,
    /// What a stacked pull request is based on.
    ///
    /// Not a parameter of [`Config::new`] either: it has a default worth
    /// having, and every caller that does not care about it would otherwise
    /// have to name it in a positional list that is already long enough to be
    /// hard to read. Set it with struct update syntax.
    pub base_strategy: BaseStrategy,
}

impl Config {
    pub fn new(
        owner: String,
        repo: String,
        remote_name: String,
        master_branch: String,
        branch_prefix: String,
        require_approval: bool,
    ) -> Self {
        let master_ref =
            GitHubBranch::new_from_branch_name(&master_branch, &remote_name, &master_branch);
        Self {
            owner,
            repo,
            remote_name,
            master_ref,
            branch_prefix,
            require_approval,
            land_with_unmet_requirements: false,
            base_strategy: BaseStrategy::default(),
        }
    }

    /// Whether GitHub's verdict on the base branch's requirements should stand
    /// in the way of a land that was passed `force`.
    ///
    /// The flag and [`Config::land_with_unmet_requirements`] are alternatives
    /// rather than an override pair: each says "land anyway", and neither has
    /// occasion to countermand the other, since nothing asks to *enforce* the
    /// requirements for a single land.
    pub fn enforce_merge_requirements(&self, force: bool) -> bool {
        !force && !self.land_with_unmet_requirements
    }

    pub fn pull_request_url(&self, number: u64) -> String {
        format!(
            "https://github.com/{owner}/{repo}/pull/{number}",
            owner = &self.owner,
            repo = &self.repo
        )
    }

    pub fn parse_pull_request_field(&self, text: &str) -> Option<u64> {
        if text.is_empty() {
            return None;
        }

        let regex = lazy_regex::regex!(r#"^\s*#?\s*(\d+)\s*$"#);
        let m = regex.captures(text);
        if let Some(caps) = m {
            return Some(caps.get(1).unwrap().as_str().parse().unwrap());
        }

        let regex = lazy_regex::regex!(
            r#"^\s*https?://github.com/([\w\-\.]+)/([\w\-\.]+)/pull/(\d+)([/?#].*)?\s*$"#
        );
        let m = regex.captures(text);
        if let Some(caps) = m
            && self.owner == caps.get(1).unwrap().as_str()
            && self.repo == caps.get(2).unwrap().as_str()
        {
            return Some(caps.get(3).unwrap().as_str().parse().unwrap());
        }

        None
    }

    pub fn get_new_branch_name(&self, existing_ref_names: &HashSet<String>, title: &str) -> String {
        self.find_unused_branch_name(existing_ref_names, &slugify(title))
    }

    pub fn get_base_branch_name(
        &self,
        existing_ref_names: &HashSet<String>,
        title: &str,
    ) -> String {
        self.find_unused_branch_name(
            existing_ref_names,
            &format!("{}.{}", self.master_ref.branch_name(), &slugify(title)),
        )
    }

    /// Whether `branch_name` is in the namespace jj-spr generates branches in,
    /// and is not the master branch.
    ///
    /// This is not the test for whether a branch may be deleted — see
    /// [`Self::is_synthetic_base_branch`], which is narrower. A pull request
    /// head branch answers `true` here and must never be taken away while the
    /// pull request is open, all the more so under [`BaseStrategy::Linear`],
    /// where it is also what the pull request above is based on.
    pub fn is_spr_branch(&self, branch_name: &str) -> bool {
        branch_name.starts_with(&self.branch_prefix) && branch_name != self.master_ref.branch_name()
    }

    /// Whether `branch_name` names a base branch jj-spr generated to carry a
    /// stacked pull request's parent tree, as [`Self::get_base_branch_name`]
    /// names one.
    ///
    /// Such a branch belongs to the one pull request based on it, which is why
    /// only such a branch may be given a derived base commit or deleted when a
    /// pull request stops pointing at it. Under [`BaseStrategy::Linear`] a
    /// stacked pull request's base is instead the head branch of the pull
    /// request below, which is not ours to write to or take away: doing either
    /// would disturb that pull request, and deleting it would close it.
    pub fn is_synthetic_base_branch(&self, branch_name: &str) -> bool {
        // What tells the two apart is the `.` that
        // [`Self::get_base_branch_name`] puts between the master branch name
        // and the slug: `slugify` drops every `.`, so the slug a head branch is
        // named after can never contain one. The master branch name itself is
        // deliberately not matched on — a repository that has renamed its
        // default branch still owns the base branches it made under the old
        // name, and they still have to be cleaned up.
        let Some(name) = branch_name.strip_prefix(&self.branch_prefix) else {
            return false;
        };

        self.is_spr_branch(branch_name) && name.contains('.')
    }

    fn find_unused_branch_name(&self, existing_ref_names: &HashSet<String>, slug: &str) -> String {
        let remote_name = &self.remote_name;
        let branch_prefix = &self.branch_prefix;
        let mut branch_name = format!("{branch_prefix}{slug}");
        let mut suffix = 0;

        loop {
            let remote_ref = format!("refs/remotes/{remote_name}/{branch_name}");

            if !existing_ref_names.contains(&remote_ref) {
                return branch_name;
            }

            suffix += 1;
            branch_name = format!("{branch_prefix}{slug}-{suffix}");
        }
    }

    pub fn new_github_branch_from_ref(&self, ghref: &str) -> Result<GitHubBranch> {
        GitHubBranch::new_from_ref(ghref, &self.remote_name, self.master_ref.branch_name())
    }

    pub fn new_github_branch(&self, branch_name: &str) -> GitHubBranch {
        GitHubBranch::new_from_branch_name(
            branch_name,
            &self.remote_name,
            self.master_ref.branch_name(),
        )
    }
}

pub enum AuthTokenSource {
    Config(String),
    GitHubCLI(String),
}

impl AuthTokenSource {
    pub fn token(&self) -> &String {
        match self {
            AuthTokenSource::Config(token) | AuthTokenSource::GitHubCLI(token) => token,
        }
    }
}

pub fn get_auth_token(git_config: &git2::Config) -> Option<String> {
    get_auth_token_with_source(git_config).map(|v| v.token().to_owned())
}

pub fn get_auth_token_with_source(git_config: &git2::Config) -> Option<AuthTokenSource> {
    // Prefer the configured token if it exists
    if let Some(token) = get_config_value("spr.githubAuthToken", git_config) {
        return Some(AuthTokenSource::Config(token));
    }

    // Try to get a token from the gh CLI
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdout(std::process::Stdio::piped())
        .output()
        .ok()?;

    if output.status.success() {
        Some(AuthTokenSource::GitHubCLI(
            String::from_utf8(output.stdout).ok()?.trim().to_owned(),
        ))
    } else {
        None
    }
}

// Helper function to get config value from jj first, then git
pub fn get_config_value(key: &str, git_config: &git2::Config) -> Option<String> {
    // Try jj config first
    if let Ok(output) = std::process::Command::new("jj")
        .args(["config", "get", key])
        .output()
        && output.status.success()
        && let Ok(value) = String::from_utf8(output.stdout)
    {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // Fall back to git config
    git_config.get_string(key).ok()
}

pub fn get_config_bool(key: &str, git_config: &git2::Config) -> Option<bool> {
    // Try jj config first
    if let Ok(output) = std::process::Command::new("jj")
        .args(["config", "get", key])
        .output()
        && output.status.success()
        && let Ok(value) = String::from_utf8(output.stdout)
    {
        let trimmed = value.trim().to_lowercase();
        if trimmed == "true" {
            return Some(true);
        } else if trimmed == "false" {
            return Some(false);
        }
    }

    // Fall back to git config
    git_config.get_bool(key).ok()
}

/// Helper function to set config value in jj (repo-level)
pub fn set_jj_config(key: &str, value: &str, repo_path: &std::path::Path) -> Result<()> {
    let output = std::process::Command::new("jj")
        .args(["config", "set", "--repo", key, value])
        .current_dir(repo_path)
        .output()
        .map_err(|e| crate::error::Error::new(format!("Failed to execute jj config set: {}", e)))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(crate::error::Error::new(format!(
            "jj config set failed for key '{}': {}",
            key, stderr
        )))
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    fn config_factory() -> Config {
        crate::config::Config::new(
            "acme".into(),
            "codez".into(),
            "origin".into(),
            "master".into(),
            "spr/foo/".into(),
            false,
        )
    }

    /// The check is on unless something asks for it to be off. Nothing about
    /// the default configuration should be able to turn it off by itself,
    /// because a caller with permission to bypass the base branch's
    /// requirements gets no other warning that they are doing so.
    #[test]
    fn merge_requirements_are_enforced_by_default() {
        assert!(config_factory().enforce_merge_requirements(false));
    }

    #[test]
    fn force_stops_enforcing_merge_requirements() {
        assert!(!config_factory().enforce_merge_requirements(true));
    }

    #[test]
    fn config_stops_enforcing_merge_requirements() {
        let config = Config {
            land_with_unmet_requirements: true,
            ..config_factory()
        };
        assert!(!config.enforce_merge_requirements(false));
    }

    /// Asking for the same thing twice asks for it once.
    #[test]
    fn force_and_config_together_stop_enforcing_merge_requirements() {
        let config = Config {
            land_with_unmet_requirements: true,
            ..config_factory()
        };
        assert!(!config.enforce_merge_requirements(true));
    }

    #[test]
    fn test_set_jj_config_success() {
        // Create a temporary jj repo for testing
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let path = temp_dir.path();

        // Initialize git repo first
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .expect("Failed to init git repo");

        // Initialize jj repo (colocated)
        let jj_init = std::process::Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(path)
            .output()
            .expect("Failed to init jj repo");

        if !jj_init.status.success() {
            // Skip test if jj is not available
            return;
        }

        // Test setting a config value
        let result = set_jj_config("spr.githubRepository", "test/repo", path);
        assert!(result.is_ok(), "Should successfully set config");

        // Verify the config was set
        let output = std::process::Command::new("jj")
            .args(["config", "get", "spr.githubRepository"])
            .current_dir(path)
            .output()
            .expect("Failed to get config");

        assert!(output.status.success());
        let value = String::from_utf8(output.stdout).unwrap();
        assert_eq!(value.trim(), "test/repo");
    }

    #[test]
    fn test_set_jj_config_multiple_values() {
        // Create a temporary jj repo for testing
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let path = temp_dir.path();

        // Initialize git repo first
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .expect("Failed to init git repo");

        // Initialize jj repo (colocated)
        let jj_init = std::process::Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(path)
            .output()
            .expect("Failed to init jj repo");

        if !jj_init.status.success() {
            // Skip test if jj is not available
            return;
        }

        // Set multiple config values
        assert!(set_jj_config("spr.githubRepository", "owner/repo", path).is_ok());
        assert!(set_jj_config("spr.branchPrefix", "spr/test/", path).is_ok());
        assert!(set_jj_config("spr.requireApproval", "false", path).is_ok());

        // Verify all configs were set correctly
        let output = std::process::Command::new("jj")
            .args(["config", "get", "spr.githubRepository"])
            .current_dir(path)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "owner/repo"
        );

        let output = std::process::Command::new("jj")
            .args(["config", "get", "spr.branchPrefix"])
            .current_dir(path)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "spr/test/"
        );

        let output = std::process::Command::new("jj")
            .args(["config", "get", "spr.requireApproval"])
            .current_dir(path)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "false");
    }

    #[test]
    fn test_set_jj_config_invalid_repo() {
        // Try to set config in a non-existent directory
        let result = set_jj_config(
            "spr.test",
            "value",
            std::path::Path::new("/nonexistent/path"),
        );
        assert!(result.is_err(), "Should fail for invalid repo path");
    }

    #[test]
    fn test_pull_request_url() {
        let gh = config_factory();

        assert_eq!(
            &gh.pull_request_url(123),
            "https://github.com/acme/codez/pull/123"
        );
    }

    #[test]
    fn test_parse_pull_request_field_empty() {
        let gh = config_factory();

        assert_eq!(gh.parse_pull_request_field(""), None);
        assert_eq!(gh.parse_pull_request_field("   "), None);
        assert_eq!(gh.parse_pull_request_field("\n"), None);
    }

    #[test]
    fn test_parse_pull_request_field_number() {
        let gh = config_factory();

        assert_eq!(gh.parse_pull_request_field("123"), Some(123));
        assert_eq!(gh.parse_pull_request_field("   123 "), Some(123));
        assert_eq!(gh.parse_pull_request_field("#123"), Some(123));
        assert_eq!(gh.parse_pull_request_field(" # 123"), Some(123));
    }

    #[test]
    fn test_is_spr_branch() {
        let gh = config_factory();

        assert!(gh.is_spr_branch("spr/foo/my-feature"));
        assert!(gh.is_spr_branch("spr/foo/master.my-feature"));
        assert!(!gh.is_spr_branch("master"));
        assert!(!gh.is_spr_branch("spr/bar/my-feature"));
        assert!(!gh.is_spr_branch("release-1.0"));
    }

    /// The name a base branch is generated under has to read back as one, or
    /// the branches jj-spr owns and the branches it must leave alone cannot be
    /// told apart.
    #[test]
    fn a_generated_base_branch_name_reads_as_one() {
        let gh = config_factory();
        let name = gh.get_base_branch_name(&HashSet::new(), "My Feature");

        assert!(gh.is_synthetic_base_branch(&name), "{name}");
    }

    /// A pull request head branch must not: under the linear base strategy it
    /// is what a stacked pull request is based on, and jj-spr deletes the base
    /// branches it owns.
    #[test]
    fn a_head_branch_does_not_read_as_a_base_branch() {
        let gh = config_factory();

        for title in ["My Feature", "master.my-feature", "master then more"] {
            let name = gh.get_new_branch_name(&HashSet::new(), title);
            assert!(!gh.is_synthetic_base_branch(&name), "{name}");
        }
    }

    #[test]
    fn test_is_synthetic_base_branch() {
        let gh = config_factory();

        assert!(gh.is_synthetic_base_branch("spr/foo/master.my-feature"));
        assert!(!gh.is_synthetic_base_branch("spr/foo/my-feature"));
        assert!(!gh.is_synthetic_base_branch("master"));
        // Someone else's prefix, so someone else's branch.
        assert!(!gh.is_synthetic_base_branch("spr/bar/master.my-feature"));
        assert!(!gh.is_synthetic_base_branch("master.my-feature"));
    }

    /// A repository that renames its default branch still has base branches
    /// named after the old one, and they are still jj-spr's to clean up.
    #[test]
    fn a_base_branch_from_before_a_default_branch_rename_still_reads_as_one() {
        let gh = config_factory();

        assert!(gh.is_synthetic_base_branch("spr/foo/main.my-feature"));
    }

    #[test]
    fn base_strategy_is_synthetic_by_default() {
        assert_eq!(config_factory().base_strategy, BaseStrategy::Synthetic);
        assert_eq!(BaseStrategy::default(), BaseStrategy::Synthetic);
    }

    #[test]
    fn base_strategy_parses_its_two_values() {
        assert_eq!(
            "synthetic".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::Synthetic
        );
        assert_eq!(
            "linear".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::Linear
        );
        // git config hands values over as they were written.
        assert_eq!(
            " Linear\n".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::Linear
        );
    }

    /// What `jj spr init` offers is what it writes into the configuration, so
    /// every name it can store has to be one the setting reads back — and the
    /// list it offers has to hold every strategy, or a strategy exists that
    /// nothing asks about.
    #[test]
    fn every_base_strategy_is_offered_under_a_name_that_parses_back() {
        for strategy in BaseStrategy::ALL {
            assert_eq!(strategy.as_str().parse::<BaseStrategy>().unwrap(), strategy);
        }

        for strategy in [BaseStrategy::Synthetic, BaseStrategy::Linear] {
            assert!(
                BaseStrategy::ALL.contains(&strategy),
                "{strategy:?} is not offered by `jj spr init`"
            );
        }
    }

    /// A misspelt strategy must not quietly mean the default: the two
    /// strategies build different branches, and a typo would look like the
    /// setting had no effect.
    #[test]
    fn base_strategy_rejects_anything_else() {
        let error = "lienar".parse::<BaseStrategy>().unwrap_err();

        assert!(
            error.messages().iter().any(|m| m.contains("lienar")),
            "the error should name the value it rejected: {error:?}"
        );
    }

    #[test]
    fn test_parse_pull_request_field_url() {
        let gh = config_factory();

        assert_eq!(
            gh.parse_pull_request_field("https://github.com/acme/codez/pull/123"),
            Some(123)
        );
        assert_eq!(
            gh.parse_pull_request_field("  https://github.com/acme/codez/pull/123  "),
            Some(123)
        );
        assert_eq!(
            gh.parse_pull_request_field("https://github.com/acme/codez/pull/123/"),
            Some(123)
        );
        assert_eq!(
            gh.parse_pull_request_field("https://github.com/acme/codez/pull/123?x=a"),
            Some(123)
        );
        assert_eq!(
            gh.parse_pull_request_field("https://github.com/acme/codez/pull/123/foo"),
            Some(123)
        );
        assert_eq!(
            gh.parse_pull_request_field("https://github.com/acme/codez/pull/123#abc"),
            Some(123)
        );
    }
}
