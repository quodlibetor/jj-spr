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

/// Which branch a stacked pull request asks to be merged into, and how the
/// branch it asks for is built.
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
    /// Base a stacked pull request on the pull request branch of the change
    /// below it, as [`Self::Linear`] does, and build the pull request branch as
    /// the change's own commits sitting on that branch's tip: one parent each,
    /// no merge commit anywhere.
    ///
    /// The commits a branch already carries cannot stay where they are when its
    /// base moves, so they are replayed onto the new base and the branch is
    /// force-pushed. That is what this strategy trades away — under the other
    /// two, a commit jj-spr has pushed is never rewritten — and what it buys is
    /// a branch GitHub can rebase: its stacked pull requests rebase the branch
    /// of the pull request above the one they merge, which discards merge
    /// commits and closes that pull request as empty. See [`crate::replay`].
    LinearRebase,
}

impl std::str::FromStr for BaseStrategy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "synthetic" => Ok(Self::Synthetic),
            "linear" => Ok(Self::Linear),
            "linear-rebase" => Ok(Self::LinearRebase),
            other => Err(Error::new(format!(
                "spr.baseStrategy must be 'synthetic', 'linear' or 'linear-rebase', \
                 but is '{other}'"
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
    pub const ALL: [Self; 3] = [Self::Synthetic, Self::Linear, Self::LinearRebase];

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
            Self::LinearRebase => "linear-rebase",
        }
    }

    /// Whether a stacked pull request is based on the pull request branch of the
    /// change below it, rather than on a base branch of its own.
    ///
    /// The two linear strategies differ in how the head branch is built, not in
    /// what it is based on, so everything about the base asks this rather than
    /// naming either of them.
    pub fn bases_on_the_change_below(self) -> bool {
        matches!(self, Self::Linear | Self::LinearRebase)
    }

    /// Whether a pull request branch is rebuilt from the change's own commits
    /// whenever its base moves, instead of merging the new base into what the
    /// branch already carries.
    pub fn rebases_branches(self) -> bool {
        matches!(self, Self::LinearRebase)
    }
}

/// How a pull request says which stack it belongs to.
///
/// The two ways of saying it are alternatives, not layers: GitHub draws the
/// stack itself from its stacked pull requests, and the section is a list
/// written into the pull request body for repositories where it does not. Both
/// at once would describe the same stack twice, in two places that can disagree,
/// so this is one setting with three values rather than two settings that can
/// both be on — the disjointness is the shape of the type, and there is no
/// combination left to refuse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackDisplay {
    /// Say nothing. A pull request stands on its own, and the stack is visible
    /// only in the branches.
    None,
    /// Write a `Stack` section into each pull request's body, listing the stack
    /// bottom-up with a marker on the pull request being read.
    ///
    /// The default, because it is the one that works everywhere: it asks nothing
    /// of the repository, and a stack described this way is described on any
    /// host. Turning it off is an explicit [`Self::None`].
    #[default]
    Section,
    /// Register the pull requests as a stack with GitHub's Stacked Pull Requests
    /// API and let GitHub draw it.
    ///
    /// Better where it is available — GitHub shows the stack on each pull
    /// request and in the repository's list of stacks, and offers to merge it —
    /// but it is not available everywhere. It is in public preview, and its
    /// merge-queue support was still rolling out separately as of August 2026,
    /// so a repository with a required merge queue may answer that it has no
    /// stacks at all.
    Github,
}

impl std::str::FromStr for StackDisplay {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "section" => Ok(Self::Section),
            "github" => Ok(Self::Github),
            other => Err(Error::new(format!(
                "spr.stackDisplay must be 'none', 'section' or 'github', but is '{other}'"
            ))),
        }
    }
}

impl StackDisplay {
    /// Every value, in the order `jj spr init` offers them: the one that asks
    /// most of the repository first, then the fallback, then off.
    ///
    /// Kept next to the enum rather than in `init`, so that a value added here
    /// is offered rather than quietly left out of the one place that asks about
    /// it.
    pub const ALL: [Self; 3] = [Self::Github, Self::Section, Self::None];

    /// The value `spr.stackDisplay` takes.
    ///
    /// The inverse of the [`FromStr`](std::str::FromStr) above: `jj spr init`
    /// offers these names and then stores the one that was picked, so the two
    /// directions have to agree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Section => "section",
            Self::Github => "github",
        }
    }

    /// Whether GitHub is asked to draw the stack, which is what needs each pull
    /// request based on the branch of the one below.
    pub fn draws_the_stack(self) -> bool {
        matches!(self, Self::Github)
    }

    /// Whether a `Stack` section is written into each pull request body.
    ///
    /// Nothing reads this yet; the section arrives with the change that writes
    /// it. It is named here so that the setting is whole from the start rather
    /// than growing a value later.
    pub fn writes_a_section(self) -> bool {
        matches!(self, Self::Section)
    }
}

/// The base strategy to run under, given `spr.stackDisplay` and whatever
/// `spr.baseStrategy` was set to — `None` where it was not set at all.
///
/// GitHub's stacks require each pull request's base ref to be the head ref of
/// the one below, which is what the two linear strategies build and
/// [`BaseStrategy::Synthetic`] never does. So the two settings are not
/// independent, and this is the one place that says so: resolving it here, as
/// the configuration is built, keeps every decision downstream a question about
/// the base strategy alone rather than about a combination of settings.
///
/// Asking for both [`StackDisplay::Github`] and the synthetic strategy is a
/// contradiction worth surfacing rather than resolving, because either half
/// could be the mistake. Leaving the strategy unset is not: it means no
/// preference, so it supplies one — [`BaseStrategy::LinearRebase`], the only one
/// whose branches survive GitHub merging a stack from its own interface.
/// [`BaseStrategy::Linear`] satisfies the stacks API just as well and is
/// honoured where it was asked for, with the hazard reported by
/// [`Config::stack_shape_warning`] rather than by refusing to run.
pub fn resolve_base_strategy(
    stack_display: StackDisplay,
    configured: Option<BaseStrategy>,
) -> Result<BaseStrategy> {
    match (stack_display.draws_the_stack(), configured) {
        (true, Some(BaseStrategy::Synthetic)) => Err(Error::new(
            "spr.stackDisplay = github needs a linear spr.baseStrategy: GitHub's stacked pull \
             requests require each pull request to be based on the branch of the one below it, \
             which is what the synthetic strategy does not do. Set spr.baseStrategy to \
             'linear-rebase', or set spr.stackDisplay to 'section'."
                .to_string(),
        )),
        (true, configured) => Ok(configured.unwrap_or(BaseStrategy::LinearRebase)),
        (false, configured) => Ok(configured.unwrap_or_default()),
    }
}

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
    /// Hand the whole job to GitHub's stacked pull requests: one request merges
    /// the pull request and every member of its stack below it, and GitHub
    /// retargets and rebases the members above.
    ///
    /// Never chosen by [`Self::Auto`], and not because it is worse. It lands the
    /// same changes as [`Self::Merge`] does, one squash commit each, and leaves
    /// less for the next `jj spr diff` to do — but the commit messages come from
    /// the repository's squash settings rather than from the local commit's
    /// message sections, because one request merges several pull requests and
    /// there is nowhere to put a message for each. That is a visible change to
    /// what lands, so it is asked for rather than inferred.
    ///
    /// Wants what `spr.stackDisplay = github` builds, and refuses the land where it is not
    /// there: a stack to merge, and pull request branches that survive GitHub
    /// rebasing them, which is [`BaseStrategy::LinearRebase`] alone.
    Stack,
}

impl std::str::FromStr for LandStrategy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "merge" => Ok(Self::Merge),
            "queue" => Ok(Self::Queue),
            "stack" => Ok(Self::Stack),
            other => Err(Error::new(format!(
                "spr.landStrategy must be 'auto', 'merge', 'queue' or 'stack', but is '{other}'"
            ))),
        }
    }
}

impl LandStrategy {
    /// Every strategy, in the order `jj spr init` offers them: the default
    /// first, then the two it chooses between, then the one it never does.
    ///
    /// Kept next to the enum rather than in `init`, so that a strategy added
    /// here is offered rather than quietly left out of the one place that asks
    /// about it. `init` offers [`Self::Stack`] only where the rest of the
    /// configuration can carry it; every entry here is a strategy it may write,
    /// not one it must.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Merge, Self::Queue, Self::Stack];

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
            Self::Stack => "stack",
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
    /// How a pull request says which stack it belongs to. See [`StackDisplay`].
    ///
    /// Neither value changes what `diff` pushes; each adds a step around the
    /// run. [`StackDisplay::Github`] does want a base strategy that
    /// [bases each pull request on the one below](BaseStrategy::bases_on_the_change_below),
    /// which `main.rs` sees to by resolving the two together through
    /// [`resolve_base_strategy`] — the one place that enforces it. A `Config`
    /// built by hand can hold any combination, and under
    /// [`BaseStrategy::Synthetic`] no pull request is ever chained to the one
    /// below, so no chain forms and nothing is registered. Set it with struct
    /// update syntax, for the reasons above.
    pub stack_display: StackDisplay,
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
            land_with_unmet_requirements: false,
            base_strategy: BaseStrategy::default(),
            stack_display: StackDisplay::default(),
            land_strategy: LandStrategy::default(),
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

    /// What is worth saying about the shape of the branches this configuration
    /// registers as a GitHub stack, or `None` where there is nothing to say.
    ///
    /// A stack GitHub draws is a stack GitHub offers to merge, and its stack
    /// merge rebases the head branch of the pull request above the one it
    /// merges. Under [`BaseStrategy::Linear`] that branch is a merge commit,
    /// which a rebase discards: the branch collapses onto its base and GitHub
    /// closes the pull request as empty, review and all. `jj spr land` never
    /// asks for that merge, but the button in GitHub's interface is right there,
    /// so a run that registers such a stack says so.
    ///
    /// Kept here, next to the settings it is about, rather than in
    /// [`resolve_base_strategy`]: that function's job is the one combination it
    /// refuses, and a warning is not a refusal. The combination is honoured —
    /// somebody may want the immutable branches and be content to land only
    /// through jj-spr.
    pub fn stack_shape_warning(&self) -> Option<&'static str> {
        (self.stack_display.draws_the_stack() && self.base_strategy == BaseStrategy::Linear)
            .then_some(
                "spr.baseStrategy = linear builds pull request branches out of merge commits, and \
             GitHub's stack merge rebases the branch of the pull request above the one it \
             merges, which discards them and closes that pull request as empty. Do not merge a \
             stacked pull request from GitHub's own interface; `jj spr land` is safe. \
             spr.baseStrategy = linear-rebase builds branches that survive it.",
            )
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
    /// pull request is open, all the more so under a linear strategy, where it
    /// is also what the pull request above is based on.
    pub fn is_spr_branch(&self, branch_name: &str) -> bool {
        branch_name.starts_with(&self.branch_prefix) && branch_name != self.master_ref.branch_name()
    }

    /// Whether `branch_name` names a base branch jj-spr generated to carry a
    /// stacked pull request's parent tree, as [`Self::get_base_branch_name`]
    /// names one.
    ///
    /// Such a branch belongs to the one pull request based on it, which is why
    /// only such a branch may be given a derived base commit or deleted when a
    /// pull request stops pointing at it. Under a linear strategy a stacked pull
    /// request's base is instead the head branch of the pull request below,
    /// which is not ours to write to or take away: doing either would disturb
    /// that pull request, and deleting it would close it.
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

        for strategy in [
            LandStrategy::Auto,
            LandStrategy::Merge,
            LandStrategy::Queue,
            LandStrategy::Stack,
        ] {
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
    fn base_strategy_parses_its_three_values() {
        assert_eq!(
            "synthetic".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::Synthetic
        );
        assert_eq!(
            "linear".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::Linear
        );
        assert_eq!(
            "linear-rebase".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::LinearRebase
        );
        // git config hands values over as they were written.
        assert_eq!(
            " Linear-Rebase\n".parse::<BaseStrategy>().unwrap(),
            BaseStrategy::LinearRebase
        );
    }

    /// The two questions the rest of the crate asks a strategy, and the answers
    /// that place each of them. `linear-rebase` differs from `linear` in how the
    /// head branch is built, not in what it is based on, so anything about the
    /// base has to see the two alike.
    #[test]
    fn the_strategies_answer_for_the_base_and_for_the_branch_separately() {
        assert!(!BaseStrategy::Synthetic.bases_on_the_change_below());
        assert!(BaseStrategy::Linear.bases_on_the_change_below());
        assert!(BaseStrategy::LinearRebase.bases_on_the_change_below());

        assert!(!BaseStrategy::Synthetic.rebases_branches());
        assert!(!BaseStrategy::Linear.rebases_branches());
        assert!(BaseStrategy::LinearRebase.rebases_branches());
    }

    /// The section is the default because it works everywhere; turning the
    /// description off is a thing you say, not a thing you get by omission.
    #[test]
    fn the_stack_display_is_the_section_by_default() {
        assert_eq!(config_factory().stack_display, StackDisplay::Section);
        assert_eq!(StackDisplay::default(), StackDisplay::Section);
    }

    #[test]
    fn every_stack_display_is_offered_under_a_name_that_parses_back() {
        for display in StackDisplay::ALL {
            assert_eq!(display.as_str().parse::<StackDisplay>().unwrap(), display);
        }

        for display in [
            StackDisplay::None,
            StackDisplay::Section,
            StackDisplay::Github,
        ] {
            assert!(
                StackDisplay::ALL.contains(&display),
                "{display:?} is not offered by `jj spr init`"
            );
        }
    }

    /// Only one of them describes the stack in each place, and the type is what
    /// says so: there is no value that does both, and none that is asked to.
    #[test]
    fn the_two_ways_of_describing_a_stack_are_disjoint() {
        for display in StackDisplay::ALL {
            assert!(
                !(display.draws_the_stack() && display.writes_a_section()),
                "{display:?} describes the stack twice"
            );
        }

        assert!(StackDisplay::Github.draws_the_stack());
        assert!(StackDisplay::Section.writes_a_section());
        assert!(!StackDisplay::None.draws_the_stack());
        assert!(!StackDisplay::None.writes_a_section());
    }

    /// A misspelt value must not quietly mean the default: it would look like
    /// the setting had no effect.
    #[test]
    fn the_stack_display_rejects_anything_else() {
        let error = "gh".parse::<StackDisplay>().unwrap_err();

        assert!(
            error.messages().iter().any(|m| m.contains("gh")),
            "the error should name the value it rejected: {error:?}"
        );
    }

    /// Unless GitHub is drawing the stack, the strategy is whatever was
    /// configured, and the default when nothing was. The section asks nothing of
    /// the base strategy, so it is in this group rather than the next.
    #[test]
    fn the_base_strategy_stands_on_its_own_unless_github_draws_the_stack() {
        for display in [StackDisplay::None, StackDisplay::Section] {
            assert_eq!(
                resolve_base_strategy(display, None).unwrap(),
                BaseStrategy::Synthetic
            );
            assert_eq!(
                resolve_base_strategy(display, Some(BaseStrategy::Synthetic)).unwrap(),
                BaseStrategy::Synthetic
            );
            assert_eq!(
                resolve_base_strategy(display, Some(BaseStrategy::Linear)).unwrap(),
                BaseStrategy::Linear
            );
            assert_eq!(
                resolve_base_strategy(display, Some(BaseStrategy::LinearRebase)).unwrap(),
                BaseStrategy::LinearRebase
            );
        }
    }

    /// An unset strategy is no preference, so drawing the stack supplies one
    /// rather than failing over a setting nobody wrote — and supplies the
    /// strategy whose branches survive GitHub merging the stack itself, since a
    /// stack GitHub draws is a stack GitHub offers to merge.
    #[test]
    fn drawing_the_stack_supplies_the_rebasing_strategy_when_none_was_chosen() {
        assert_eq!(
            resolve_base_strategy(StackDisplay::Github, None).unwrap(),
            BaseStrategy::LinearRebase
        );
    }

    /// Either linear strategy satisfies the stacks API, so a run under one that
    /// was asked for is not overridden — `linear` only gets the warning below.
    #[test]
    fn drawing_the_stack_agrees_with_either_linear_strategy() {
        assert_eq!(
            resolve_base_strategy(StackDisplay::Github, Some(BaseStrategy::Linear)).unwrap(),
            BaseStrategy::Linear
        );
        assert_eq!(
            resolve_base_strategy(StackDisplay::Github, Some(BaseStrategy::LinearRebase)).unwrap(),
            BaseStrategy::LinearRebase
        );
    }

    /// The hazard `linear` carries into a GitHub stack is reported, not refused:
    /// jj-spr's own land never asks for the stack merge that would trigger it,
    /// so the combination is workable as long as its owner knows.
    #[test]
    fn a_merge_shaped_stack_is_warned_about() {
        let config = Config {
            stack_display: StackDisplay::Github,
            base_strategy: BaseStrategy::Linear,
            ..config_factory()
        };

        let warning = config
            .stack_shape_warning()
            .expect("a merge-shaped stack should be warned about");
        assert!(
            warning.contains("linear-rebase"),
            "the warning should name the strategy that avoids it: {warning}"
        );
    }

    /// Nothing to warn about where nothing is registered as a stack, or where
    /// the branches survive a rebase.
    #[test]
    fn nothing_else_is_warned_about() {
        for (stack_display, base_strategy) in [
            (StackDisplay::Section, BaseStrategy::Linear),
            (StackDisplay::None, BaseStrategy::Synthetic),
            (StackDisplay::Github, BaseStrategy::LinearRebase),
        ] {
            let config = Config {
                stack_display,
                base_strategy,
                ..config_factory()
            };

            assert!(
                config.stack_shape_warning().is_none(),
                "{stack_display:?} with {base_strategy:?} should say nothing"
            );
        }
    }

    /// The one combination that cannot be honoured: it must be reported rather
    /// than resolved, because either half of it could be the mistake, and a
    /// silently overridden strategy would build branches the user did not ask
    /// for.
    #[test]
    fn drawing_the_stack_refuses_the_synthetic_strategy() {
        let error =
            resolve_base_strategy(StackDisplay::Github, Some(BaseStrategy::Synthetic)).unwrap_err();

        assert!(
            error
                .messages()
                .iter()
                .any(|m| m.contains("spr.stackDisplay") && m.contains("spr.baseStrategy")),
            "the error should name both settings: {error:?}"
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

        for strategy in [
            BaseStrategy::Synthetic,
            BaseStrategy::Linear,
            BaseStrategy::LinearRebase,
        ] {
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
