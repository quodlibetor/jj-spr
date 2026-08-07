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

- **Integration tests** in `spr/tests/` verify end-to-end workflows using temporary Git/Jujutsu repos
- **Unit tests** are colocated with implementation code
- Tests require both `jj` and `git` binaries available in PATH
- CI runs on Ubuntu with all dependencies installed

## Configuration Options

Stored in git config under `spr.*` namespace:
- `spr.githubRepository` - Owner/repo name
- `spr.branchPrefix` - Prefix for generated branches (default: `spr/`)
- `spr.requireApproval` - Require PR approval before landing
- `spr.landWithUnmetRequirements` - Land even when GitHub reports the PR blocked by its base branch's requirements (default: `false`; `jj spr land --force` does the same for one land)
- `spr.baseStrategy` - What a stacked PR is based on, and how its branch is built: `synthetic` (default) gives each PR its own generated base branch carrying the parent change's tree; `linear` bases each PR on the PR branch of the change below it, so the stack on GitHub is a chain of branches; `linear-rebase` does the same and additionally makes every PR branch a chain of single-parent commits — the change's own commits replayed onto whatever its base moved to — instead of merging the new base in. Both linear strategies want the whole stack pushed in one run: a change whose parent is not in the run keeps the base it has, and falls back to a synthetic base branch only when a base commit has to be built for it
- `spr.baseStrategy = linear-rebase` is the only strategy that force-pushes: a branch whose base moved is rewritten (`--force-with-lease`), which is what makes it survive GitHub rebasing it — GitHub's stack merge rebases the branch of the PR above the one it merges, and a merge commit does not survive that. The commits are replayed one by one, so the number of review rounds and their messages are kept even though their ids change; where a replay conflicts, or where the branch was pushed under another strategy, the branch is rebuilt as a single commit and `jj spr diff` says so. See `spr/src/replay.rs`
- `spr.stackDisplay = github` - Register the PRs a `jj spr diff` run pushes with GitHub's Stacked Pull Requests API, so GitHub draws the stack (default: `false`). Requires a linear `spr.baseStrategy`: it supplies `linear-rebase` when the strategy is unset, honours `linear` with a warning that merging the stack from GitHub's interface would destroy the PRs above, and refuses `synthetic`. Changing a stacked PR's base means dissolving the whole stack first — GitHub rejects any base change while a PR is stacked — so a run that retargets anything mints a new stack number and leaves any member it did not push unstacked. `jj spr land` dissolves the stack holding the PR before it merges anything — GitHub refuses to merge a PR a stack holds, and its own stack merge destroys the PRs above — and dissolves any stack holding a PR it retargets afterwards; run `jj spr diff` to register what is left. `jj spr close` dissolves whatever stack holds each PR it has to retarget — closing itself needs nothing, since GitHub allows it while a stack holds the PR and keeps the closed PR in the stack — and reports what it left unstacked; to register a stack again, take the closed change out of the local chain (abandon it or fold it into a neighbour) and run `jj spr diff` over what is left
- `spr.landStrategy` - How `jj spr land` lands a pull request: `merge` squash-merges it there and then, `queue` puts it in the merge queue GitHub keeps for the default branch, and `auto` (the default) asks GitHub which of the two that branch allows
- `spr.githubHost` - Custom GitHub Enterprise host
- `spr.githubToken` - GitHub API token (typically stored via `jj spr init`)
