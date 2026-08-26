# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Super Pull Requests (SPR) is a Rust-based CLI tool (`jj-spr`) that bridges Jujutsu's change-based workflow with GitHub's pull request model. It's the power tool for Jujutsu + GitHub workflows, enabling amend-friendly single PRs and effortless stacked PRs.

**Key Value Propositions:**
- **Amend-friendly workflow**: Users can amend freely locally using Jujutsu's natural workflow while maintaining clean, incremental diffs for reviewers on GitHub
- **Effortless stacking**: Supports both independent and dependent PR stacks with automatic rebase handling and flexible landing order

**Key Architecture Concepts:**
- **Change ID-based**: Uses Jujutsu's stable change IDs instead of Git commit hashes
- **Stacking support**: Creates dependent PRs with proper base branch handling
- **Colocated repositories**: Requires both `.jj` and `.git` directories (Jujutsu colocated with Git)
- **GitHub API integration**: Uses both REST API (via `octocrab`) and GraphQL API for PR management

## Development Commands

### Building and Testing
```bash
# Build the project
cargo build

# Build release version
cargo build --release

# Run all tests (requires jj and git installed)
cargo test

# Run specific test
cargo test test_name

# Run integration tests
cargo test --test '*'
```

### Code Quality
```bash
# Format code
cargo fmt

# Check formatting without modifying
cargo fmt --all -- --check

# Run clippy (treat warnings as errors in CI)
cargo clippy --all-features --all-targets -- -D warnings

# Run clippy for development (warnings allowed)
cargo clippy --all-features --all-targets
```

### Nix Development (if using Nix)
```bash
# Enter development shell with all dependencies
nix develop

# Build with Nix
nix build

# Check flake
nix flake check

# Update flake dependencies
nix flake update
```

## Core Architecture

### Module Structure

The codebase follows a library + binary structure:

- **`spr/src/lib.rs`**: Public module declarations
- **`spr/src/main.rs`**: CLI entrypoint using `clap`
- **`spr/src/commands/`**: Individual command implementations (diff, land, list, amend, close, etc.)
- **`spr/src/jj.rs`**: Jujutsu integration layer (executes `jj` commands, manages change IDs)
- **`spr/src/git.rs`**: Git operations via `git2` crate (branch management, commit creation)
- **`spr/src/github.rs`**: GitHub API client (PR creation/updates, GraphQL queries)
- **`spr/src/config.rs`**: Configuration management (reads from git config)
- **`spr/src/message.rs`**: Commit message parsing and formatting (Summary, Reviewers, etc.)
- **`spr/src/revision_utils.rs`**: Revision parsing and resolution
- **`spr/src/output.rs`**: Terminal output formatting
- **`spr/src/error.rs`**: Error types and handling

### Key Design Patterns

1. **Change-based workflow**: Each Jujutsu change (identified by a change ID) maps to one GitHub PR. The PR branch name includes the change ID to maintain the link even after rebases.

2. **Stacked PR handling**: When creating stacked PRs, the tool creates intermediate branches for each change in the stack. The base branch of PR #2 points to the PR branch of PR #1, allowing independent reviews and landing.

3. **Commit message structure**: Commit messages are parsed into sections (Summary, Reviewers, etc.) which are stored in both the local Jujutsu change and the GitHub PR. The tool maintains bidirectional sync.

4. **Git/Jujutsu bridge**: While Jujutsu is the primary interface, the tool uses Git operations under the hood for branch creation and pushing to GitHub, since GitHub only understands Git.

## Important Constraints

- **Requires colocated repository**: Must have both `.jj/` and `.git/` directories
- **GitHub-only**: Only works with GitHub (not GitLab, Bitbucket, etc.)
- **Stable change IDs**: Relies on Jujutsu's change IDs remaining stable across rebases
- **Async runtime**: Uses Tokio for async operations (GitHub API calls)
- **Configuration via git config**: Settings are stored in `.git/config` using `spr.*` keys

## Dependencies

### Runtime Requirements
- `jj` (Jujutsu CLI) must be in PATH
- `git` must be in PATH
- GitHub Personal Access Token for API access

### Key Rust Dependencies
- `clap` - CLI argument parsing with derive macros
- `git2` - Git operations via libgit2
- `octocrab` - GitHub REST API client
- `graphql_client` - GitHub GraphQL API client
- `tokio` - Async runtime
- `dialoguer` - Interactive prompts

## Common Workflows

### Creating/Updating PRs
1. User runs `jj spr diff -r <revision>`
2. Tool resolves revision to change ID and Git commit hash
3. Parses commit message into sections
4. Creates/updates Git branch named with change ID
5. Pushes branch to GitHub
6. Creates/updates PR via GitHub API
7. Updates local commit message with PR number

### Landing PRs
1. User runs `jj spr land -r <revision>`
2. Tool finds associated PR by change ID
3. Verifies PR is approved (if configured)
4. Squash-merges PR on GitHub
5. Cleans up remote branch

### Stacked PRs
1. User creates multiple changes with parent-child relationships
2. Runs `jj spr diff --all` or `jj spr diff -r main..@`
3. Tool creates PRs in order, each with base pointing to previous PR's branch
4. When landing, only the bottom PR is merged; others remain valid

## Testing Strategy

- **Unit tests** are colocated with implementation code
- **Integration tests** in `spr/tests/` verify end-to-end workflows using temporary Git/Jujutsu repos
- Tests require both `jj` and `git` binaries available in PATH
- CI runs on Ubuntu with all dependencies installed

### Where a test about GitHub belongs

Three suites, and the line between the last two is what jj-spr decides versus what GitHub does:

- `spr/tests/fake_github_test.rs` — runs `diff`, `land` and `close` against an in-process fake GitHub (behind the `GitHubApi` trait) with a **bare repository standing in for the remote**, so every branch assertion is real git and needs no network. This is where anything jj-spr decides for itself belongs: branch shapes, which base a PR gets, which calls are made in which order, what is refused, what lands on the default branch. ~2s per test, and it is where new tests should go by default.
- `spr/tests/github_e2e_test.rs` — the live suite, skipped unless `E2E_TEST_REPO` is set (`quodlibetor/spr-private-tests`), run with `--test-threads=1`. Only facts about GitHub belong here: what a retarget does, what a deleted base branch does, what its stack merge does to the PRs above, what its stacks API accepts, what it refuses. ~40s per test.
- **The live suite is what verifies the fake, and that linkage is compile-checked.** Every GitHub rule the fake leans on is a variant of `github::GitHubRule`; `contract_test_for` in the live suite maps each variant to the test that pins it in a total `match`, so a new rule does not compile until a live test is named, and `every_github_rule_has_a_contract_test` (which needs no network) fails if that name stops matching a function. So `cargo test --test github_e2e_test` with `E2E_TEST_REPO` set is the answer to "is the fake still telling the truth?".
- **A fake with a wrong rule is worse than no test.** Reproduce a rule in the fake only alongside a `GitHubRule` variant and a live test; where a rule is deliberately *not* reproduced (the fake does not rebase stack-merge survivors), say so where it would matter, and keep the assertion that needs it in the live suite.
- Prefer sharing a rule over restating it: `github::base_branch_to_take_away` exists so the real client and the fake apply one statement of "which base branch is ours to delete" rather than two.
- Mutation-test a new harness rather than trusting it: break the production rule it is supposed to catch and check that the right test fails. Every rule enforced by this fake was checked that way.

## Configuration Options

Stored in git config under `spr.*` namespace:
- `spr.githubRepository` - Owner/repo name
- `spr.branchPrefix` - Prefix for generated branches (default: `spr/`)
- `spr.requireApproval` - Require PR approval before landing
- `spr.landWithUnmetRequirements` - Land even when GitHub reports the PR blocked by its base branch's requirements (default: `false`; `jj spr land --force` does the same for one land)
- `spr.baseStrategy` - What a stacked PR is based on, and how its branch is built: `synthetic` (default) gives each PR its own generated base branch carrying the parent change's tree; `linear` bases each PR on the PR branch of the change below it, so the stack on GitHub is a chain of branches; `linear-rebase` does the same and additionally makes every PR branch a chain of single-parent commits — the change's own commits replayed onto whatever its base moved to — instead of merging the new base in. Both linear strategies reach past the revisions a run was given, because which change a PR is stacked on is a fact about the local chain rather than about the revset — see `spr/src/neighbourhood.rs`. Downwards is context: the chain of PRs below the run's bottom change supplies its base branch and is registered as part of the same stack, and nothing down there is pushed. Upwards is work: the changes stacked on the run that already have PRs are pushed too, since a run that moves a base moves their ground. Both walks stop at the first change with no PR (so a run never opens one that was not asked for) and at a fork; `--cherry-pick` turns both off. A synthetic base branch is still the fallback where the change below has no open PR, or where its branch has fallen behind the change as it is locally — `jj spr diff` names the PR it could not build on
- `spr.baseStrategy = linear-rebase` is the only strategy that force-pushes: a branch whose base moved is rewritten (`--force-with-lease`), which is what makes it survive GitHub rebasing it — GitHub's stack merge rebases the branch of the PR above the one it merges, and a merge commit does not survive that. The commits are replayed one by one, so the number of review rounds and their messages are kept even though their ids change; where a replay conflicts, or where the branch was pushed under another strategy, the branch is rebuilt as a single commit and `jj spr diff` says so. See `spr/src/replay.rs`
- `spr.githubStacks` - Register the PRs a `jj spr diff` run pushes with GitHub's Stacked Pull Requests API, so GitHub draws the stack (default: `false`). Requires a linear `spr.baseStrategy`: it supplies `linear-rebase` when the strategy is unset, honours `linear` with a warning that merging the stack from GitHub's interface would destroy the PRs above, and refuses `synthetic`. Changing a stacked PR's base means dissolving the whole stack first — GitHub rejects any base change while a PR is stacked — so a run that retargets anything mints a new stack number; what goes back is the chain the run ends up with — the PRs it pushed plus the ones below them it is chained to — and any other member of the dissolved stack is left unstacked. `jj spr land` dissolves the stack holding the PR before it merges anything — GitHub refuses to merge a PR a stack holds, and its own stack merge destroys the PRs above — and dissolves any stack holding a PR it retargets afterwards; run `jj spr diff` to register what is left. `jj spr close` dissolves whatever stack holds each PR it has to retarget — closing itself needs nothing, since GitHub allows it while a stack holds the PR and keeps the closed PR in the stack — and reports what it left unstacked; to register a stack again, take the closed change out of the local chain (abandon it or fold it into a neighbour) and run `jj spr diff` over what is left
- `spr.landStrategy` - How `jj spr land` lands a pull request: `merge` squash-merges it there and then, `queue` puts it in the merge queue GitHub keeps for the default branch, `auto` (the default) asks GitHub which of the two that branch allows, and `stack` hands the whole chain to GitHub's stacked pull requests — one `PUT /pulls/{n}/merge-async` merges the PR and every stack member below it, and GitHub retargets and rebases the ones above. `jj spr land --stack` / `--no-stack` choose for one land
- `spr.landStrategy = stack` needs `spr.baseStrategy = linear-rebase` and refuses the land without it (GitHub's rebase of the PR above would destroy a merge-shaped branch), refuses a PR in no stack, and refuses a stack that would merge more than the land is for — a stack merge takes everything below the PR it is given and cannot be narrowed. `auto` never chooses it because GitHub words each squash commit from the repository's squash settings rather than from the local commit's message sections
- `spr.githubHost` - Custom GitHub Enterprise host
- `spr.githubToken` - GitHub API token (typically stored via `jj spr init`)
