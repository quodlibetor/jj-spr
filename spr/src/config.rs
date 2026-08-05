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

/// How `jj spr land` asks GitHub to land a pull request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LandStrategy {
    /// Ask GitHub which of the two below the default branch allows, and use
    /// that one.
    ///
    /// The default, and the answer for nearly every repository: a branch with a
    /// merge queue takes no merge that does not go through it, and a branch
    /// without one has no queue to join, so the branch decides and there is
    /// nothing left to configure.
    #[default]
    Auto,
    /// Squash-merge the pull request now.
    Merge,
    /// Put the pull request in the merge queue of the default branch, and leave
    /// the merging to GitHub.
    Queue,
}

impl std::str::FromStr for LandStrategy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "merge" => Ok(Self::Merge),
            "queue" => Ok(Self::Queue),
            other => Err(Error::new(format!(
                "spr.landStrategy must be 'auto', 'merge' or 'queue', but is '{other}'"
            ))),
        }
    }
}

impl LandStrategy {
    /// Every strategy, in the order `jj spr init` offers them: the default
    /// first, then the two it chooses between.
    ///
    /// Kept next to the enum rather than in `init`, so that a strategy added
    /// here is offered rather than quietly left out of the one place that asks
    /// about it.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Merge, Self::Queue];

    /// The value `spr.landStrategy` takes for this strategy.
    ///
    /// The inverse of the [`FromStr`](std::str::FromStr) above, and here rather
    /// than spelled out wherever a strategy is written: `jj spr init` offers
    /// these names and then stores the one that was picked, so the two
    /// directions have to agree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Merge => "merge",
            Self::Queue => "queue",
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
    /// How a land asks GitHub to land a pull request. See [`LandStrategy`].
    pub land_strategy: LandStrategy,
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
            land_strategy: LandStrategy::default(),
        }
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
    fn test_land_strategy_default_is_auto() {
        assert_eq!(config_factory().land_strategy, LandStrategy::Auto);
        assert_eq!(LandStrategy::default(), LandStrategy::Auto);
    }

    #[test]
    fn test_land_strategy_parses_each_name() {
        assert_eq!("auto".parse::<LandStrategy>().unwrap(), LandStrategy::Auto);
        assert_eq!(
            "merge".parse::<LandStrategy>().unwrap(),
            LandStrategy::Merge
        );
        assert_eq!(
            "queue".parse::<LandStrategy>().unwrap(),
            LandStrategy::Queue
        );
        // Config files are written by hand, so neither surrounding space nor
        // capitalisation is a reason to refuse one.
        assert_eq!(
            " Queue\n".parse::<LandStrategy>().unwrap(),
            LandStrategy::Queue
        );
    }

    /// What `jj spr init` offers is what it writes into the configuration, so
    /// every name it can store has to be one the setting reads back — and the
    /// list it offers has to hold every strategy, or a strategy exists that
    /// nothing asks about.
    #[test]
    fn every_land_strategy_is_offered_under_a_name_that_parses_back() {
        for strategy in LandStrategy::ALL {
            assert_eq!(strategy.as_str().parse::<LandStrategy>().unwrap(), strategy);
        }

        for strategy in [LandStrategy::Auto, LandStrategy::Merge, LandStrategy::Queue] {
            assert!(
                LandStrategy::ALL.contains(&strategy),
                "{strategy:?} is not offered by `jj spr init`"
            );
        }
    }

    #[test]
    fn test_land_strategy_rejects_anything_else() {
        let error = "merge-queue".parse::<LandStrategy>().unwrap_err();
        // The value that was rejected belongs in the message: the setting can
        // come from any of several config files, and knowing which word to look
        // for is most of finding the one that holds it.
        assert!(
            error.messages().iter().any(|m| m.contains("merge-queue")),
            "expected the rejected value in {:?}",
            error.messages()
        );
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
