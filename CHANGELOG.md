# Changelog

All notable changes to Super Pull Requests will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- jj-spr now works in workspaces that aren't colocated.
- The first commit in a pull reuqest now uses the local commit's description.
- `jj spr diff` remembers when PRs were created as cherry picks so
  `--cherry-pick` doesn't need to be specified each time the PR is updated.
- `jj spr diff` retargets a pull request at the default branch once its commit
  sits directly on that branch, and deletes the synthetic base branch the pull
  request used to point at. The branch is only deleted after GitHub confirms
  the retarget, because GitHub closes a pull request whose base branch
  disappears, and only branches under `spr.branchPrefix` are deleted.
- `jj spr land` retargets the pull requests stacked on the one it lands at the
  default branch, and deletes the base branches they pointed at, instead of
  leaving that until the next `jj spr diff`.
- `jj spr land` refuses to land a pull request that GitHub reports as blocked
  by its base branch: a required check failing or not yet started, a missing
  review, an unsatisfied rule. It previously asked only whether the branches
  conflicted, so a caller able to bypass a protected branch could land a pull
  request whose CI had not started. `--force`, or the
  `spr.landWithUnmetRequirements` setting, lands anyway.
- `spr.baseStrategy` chooses what a stacked pull request is based on. The
  default, `synthetic`, keeps giving each one a generated base branch carrying
  the parent change's tree. `linear` bases it on the pull request branch of the
  change below instead, so the stack on GitHub is a chain of branches and no
  base branches are generated. Both push the whole stack forward only: jj-spr
  still never force-pushes. A change whose parent is not in the same run keeps
  the base its pull request has, and falls back to a synthetic base branch only
  where that run has to build a base commit for it — as a change pushed as a
  cherry-pick, or stacked on one, always does.

- `spr.stackDisplay` chooses how a pull request says which stack it belongs to.
  There are two ways of saying it and they are alternatives, not layers — two
  descriptions of the same stack can disagree — so it is one setting with three
  values rather than two that can both be on. `section`, the default, writes a
  `Stack` list into each pull request's body; `github` registers the pull
  requests with GitHub's stacked pull requests and lets GitHub draw the stack;
  `none` says nothing. Turning the description off is a thing you say, not a
  thing you get by omission.

  `github` needs `spr.baseStrategy = linear` — GitHub requires each pull
  request in a stack to be based on the branch of the one below — and supplies
  it where the strategy was not set, rather than overriding one that was.
  GitHub refuses to change the base branch of a pull request that is in a
  stack, so a run that has to retarget one takes the whole stack apart and
  registers the pull requests it pushed as a new one, under a new number.
  `jj spr land` and `jj spr close` do not know about stacks yet and have to
  have the stack taken apart by hand first. `--dry-run` reports what the run
  would register. `jj spr init` asks for the setting right after the base
  strategy, offering `github` only where the strategy picked can carry a
  stack.

### Fixed

- `jj spr close` no longer closes the pull requests around the one it is asked
  to close. It asks GitHub which open pull requests are based on the closed
  one's head branch, points them at the closed pull request's own base — so
  each picks up the closed changes, but not those of the pull requests below it
  that are still under review — and only then deletes the head branch, keeping
  it when one of them could not be moved. It also no longer deletes a base
  branch jj-spr did not generate: under `spr.baseStrategy = linear` that branch
  is the head branch of the pull request below, so deleting it closed that pull
  request, and a base branch set by hand went the same way. A generated base
  branch is kept too while any open pull request still targets it, which the
  ones just retargeted onto it are the usual reason for.

## [0.1.0] - 2025-11-15

### Added

- Initial release of Super Pull Requests (SPR)
- Power tool for Jujutsu + GitHub workflows
- Amend-friendly single PR workflow: Amend freely in jj, review cleanly on GitHub
- Effortless stacked PR support: Independent or dependent changes with automatic rebase handling
- Change-based workflow using Jujutsu's stable change IDs
- Commands: `diff`, `land`, `list`, `close`, `amend`
- Cherry-pick mode for independent changes
- Automatic PR updates without force-push confusion
- Support for both single PRs and stacked PRs
- GitHub API integration via REST and GraphQL
- Comprehensive documentation and guides

### Changed

- Rebranded from "jj-spr (Jujutsu Stacked Pull Requests)" to "Super Pull Requests"
- Version reset to 0.1.0 for official release
- Updated project metadata and repository information

[unreleased]: https://github.com/LucioFranco/jj-spr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/LucioFranco/jj-spr/releases/tag/v0.1.0
