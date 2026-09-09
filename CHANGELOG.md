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
- `jj spr list` has a `Merge` column reporting what stands between each pull
  request and landing: whether it is a draft, whether its branches conflict,
  and how its checks are doing. A check that fails without blocking the merge
  — one the base branch does not require — is called out separately from one
  that does.
- `jj spr list --format slack` prints the listing as a Markdown bullet list to
  paste into a chat message asking for reviews: one bullet per pull request,
  its title under an emoji for where its review stands, and its URL on the
  line below. `--format slack-links` prints the same list with each title
  made a terminal hyperlink instead, keeping every pull request to one line.
  The default format can be set with `spr.listFormat`.
- `jj spr list --copy` puts the listing on the clipboard as well as printing
  it, as HTML with a real link per pull request where the format has links to
  carry. No terminal turns a hyperlink back into a link when you copy it —
  every copy path writes plain text — so this is what gets the links into a
  chat message. The plain-text flavour that goes alongside spells the URLs
  out, since the escapes that draw a hyperlink on screen paste as rubbish.

### Changed

- `jj spr list` now lists pull requests in the order of the local changes,
  newest first, so a stack reads the way `jj log` prints it instead of in
  whatever order GitHub returned. Pull requests with no local change are
  listed last, and when there is more than one stack to tell apart a `Stack`
  column marks where each one starts and ends.

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
