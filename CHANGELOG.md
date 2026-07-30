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
