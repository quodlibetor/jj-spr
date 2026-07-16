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
- `jj spr land` lands the pull requests below the one it was asked for, bottom
  first, instead of merging that one alone. A pull request branch carries the
  local stack under it, so squash-merging one from the middle of a stack used to
  put every change below it on the default branch as well — inside that one
  squash, under that one's title — while the pull requests those changes belong
  to stayed open with nothing left to show. Each now lands as its own commit and
  is closed by the merge that carried it, which is also what GitHub's own
  `gh stack merge` does with a stack. A change below the one being landed that
  has no pull request refuses the land, because passing over it would land it
  with nothing on GitHub to say so. A pull request pushed with `--cherry-pick`
  carries its change on its own, so nothing below it is landed. Where the
  default branch has a merge queue this needs `--wait`, since each pull request
  has to be merged before the next can be queued.
- `jj spr land` puts a pull request in the merge queue where the default branch
  has one, instead of asking for a merge GitHub would refuse. The pull request
  is retargeted at the default branch first, as it is before a squash-merge. A
  queued land stops there: GitHub merges later, so the pull request branches are
  left in place — the queue merges the pull request branch — and `jj spr
  cleanup` removes them once it has. `--queue` and `--no-queue` choose for one
  land.
- `jj spr land --wait` stays until the merge queue has merged the pull request,
  and then deletes the branches it used and fetches what landed, the way a
  squash-merging land does. It gives up if GitHub takes the pull request out of
  the queue without merging it, which is what a queue does to one whose checks
  fail on the merged result.
- `spr.landStrategy` chooses how `jj spr land` lands a pull request. `merge`
  squash-merges it there and then, which is what jj-spr has always done, and
  `queue` puts it in the merge queue GitHub keeps for the default branch. The
  default, `auto`, asks GitHub which of the two that branch allows, so a
  repository that requires a merge queue needs no configuration at all. `stack`
  hands the whole chain to GitHub's stacked pull requests instead — see
  `jj spr land --stack` above, which is the same thing for one land.
  `jj spr init` asks for it, offering `stack` only where the base strategy and
  `spr.stackDisplay` it has already asked about can carry it.
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
  base branches are generated. Both push the whole stack forward only, never
  rewriting a commit they have pushed — which is what `linear-rebase`, below,
  gives up. A change whose parent is not in the same run keeps
  the base its pull request has, and falls back to a synthetic base branch only
  where that run has to build a base commit for it — as a change pushed as a
  cherry-pick, or stacked on one, always does.

- `spr.baseStrategy = linear-rebase` bases a stacked pull request on the pull
  request branch of the change below it, as `linear` does, and builds that pull
  request's own branch as a chain of single-parent commits on it: the change's
  commits replayed onto whatever its base moved to, with no merge commit
  anywhere. It is the one strategy that force-pushes, and the one whose branches
  survive being rebased — which is what makes a stack safe to merge from
  GitHub's own interface, where GitHub rebases the branch of the pull request
  above the one it merges.

  A branch whose base has not moved is added to and pushed as ever, so an
  ordinary amend is an ordinary push. A base that has moved — the change below
  was amended, the change was rebased onto a newer default branch — costs a
  replay: each commit the branch carried is put on the new base in turn, keeping
  its message, author and timestamps, and the branch is pushed with
  `--force-with-lease` against the commit GitHub reported at the start of the
  run. The review rounds are therefore kept, with new commit ids; GitHub's
  `Files changed` is unaffected, and its per-commit review comments are not.
  Where a replay conflicts, or where the branch was pushed under another
  strategy and so has merge commits with no history to read, the branch is
  rebuilt as a single commit carrying the change — and `jj spr diff` says which
  of the two happened.

  `jj spr init` offers it alongside the other two, and names it as the one to
  pick if you want GitHub to draw and merge your stacks.

- `jj spr diff` writes a `Stack` section into each pull request's body listing
  the stack it belongs to, marking the one being read — the `section` value of
  `spr.stackDisplay`, and its default. The section is generated rather than
  authored: `jj spr amend` does not write it back into your commit message and
  it never reaches the merge commit. A pull request not in a stack of at least
  two does not get one, and neither does one pushed with `--cherry-pick`, which
  carries its change alone. The stack described is the repository's rather than
  the run's — from where your chain leaves the master branch up through the top
  change's descendants — so pushing one change brings its neighbours' sections
  up to date, and a pull request reads the same however it was addressed.
  Choosing `github` or `none` takes existing sections away rather than leaving
  them to rot.

- `spr.stackDisplay` chooses how a pull request says which stack it belongs to.
  There are two ways of saying it and they are alternatives, not layers — two
  descriptions of the same stack can disagree — so it is one setting with three
  values rather than two that can both be on. `section`, the default, writes a
  `Stack` list into each pull request's body; `github` registers the pull
  requests with GitHub's stacked pull requests and lets GitHub draw the stack;
  `none` says nothing. Turning the description off is a thing you say, not a
  thing you get by omission.

  `github` needs a linear `spr.baseStrategy` — GitHub requires each pull
  request in a stack to be based on the branch of the one below — and supplies
  `linear-rebase` where the strategy was not set, rather than overriding one
  that was. Under `linear` it says once per run what merging the stack from
  GitHub's interface would do to the pull requests above. GitHub refuses to
  change the base branch of a pull request that is in a stack, so a run that
  has to retarget one takes the whole stack apart and registers the pull
  requests it pushed as a new one, under a new number. `--dry-run` reports what
  the run would register. `jj spr init` asks for the setting right after the
  base strategy, offering `github` only where the strategy picked can carry a
  stack.

- `jj spr land` works under `spr.stackDisplay = github`. It takes the GitHub stack
  holding the pull request apart before it merges anything, and then merges
  that pull request on its own — which is what it does without the setting, and
  what leaves the pull requests above it untouched. It has no choice about the
  first part: GitHub refuses to merge a pull request a stack holds, and points
  at its own stack merge instead. It says which stack it dissolved and which
  pull requests are left unstacked; `jj spr diff` afterwards registers what is
  left, under a new number.

  Taking a stack apart cannot be undone, so a land that GitHub is going to
  refuse asks first where it can. Landing the bottom pull request of a stack
  moves no base branch, so GitHub's answer is the same before the stack comes
  apart as after it — and a land it turns down for the ordinary reasons, a
  required check still running or a conflict, leaves the stack standing. Higher
  up the stack the base has to move onto the default branch before GitHub will
  answer about the merge at all, and moving it means dissolving the stack, so
  there the refusal still costs the stack.

  It does not take that offer up unless asked to with `--stack` (below), and
  not because of what the stack merge lands: merging everything below the pull
  request asked for is right, and `jj spr land` does it too. The reason is what
  the stack merge does afterwards. It rebases the head branch of the pull
  request *above* onto its new base, and under `spr.baseStrategy = synthetic` or
  `linear` the branches
  jj-spr pushes are merge commits that a rebase discards, so that branch
  collapses onto its base and GitHub closes the pull request as having no
  changes, review and all. For the same reason, under those two strategies, do
  not merge a stacked pull request from GitHub's own interface while
  GitHub is drawing the stack. Under `spr.baseStrategy = linear-rebase` — what
  `spr.stackDisplay = github` selects for itself — the branches are chains of ordinary
  commits, which survive that rebase, so merging from GitHub is safe there.

- `jj spr land --stack`, and `spr.landStrategy = stack`, land a whole chain
  through GitHub's stacked pull requests: one request merges the pull request
  and every member of its stack below it, one squash commit each, and GitHub
  moves the pull requests above onto the default branch and rebases their
  branches itself. The stack survives, so nothing has to be registered again,
  and the pull requests above come out already showing only their own changes —
  which is the one thing a land otherwise leaves for the next `jj spr diff` to
  put right. `--no-stack` merges one at a time for a single land.

  It needs `spr.baseStrategy = linear-rebase`, and refuses the land without it:
  the rebase GitHub gives the branch above discards a branch built out of merge
  commits, and GitHub then closes that pull request as empty. It also refuses a
  pull request that is in no stack, and one whose stack would merge something
  the land is not for — a stack merge takes everything below the pull request it
  is given, and there is no asking for less, so a stack holding an open pull
  request below the bottom of the local chain would land that too.

  `auto` never chooses it, and not because it is worse: it lands the same
  changes, and less is left over afterwards. The commit messages are what
  differ. One request merges several pull requests, so there is nowhere to put a
  message for each, and GitHub words them from the repository's own squash
  settings rather than from the local commit's message sections the way
  `jj spr land` does when it merges a pull request itself. That is a visible
  change to what lands, so it is asked for rather than inferred.

- `jj spr close` works under `spr.stackDisplay = github`. Closing itself needs nothing:
  GitHub allows it while a stack holds the pull request, and keeps the closed
  one in the stack. Pointing the pull requests above at the closed one's base
  is what a stack refuses, so closing takes apart whatever stack holds each of
  them first — usually the one stack they were all in. It says which stacks it
  dissolved and which pull requests are left unstacked, not counting the one it
  just closed. To get a stack again, take the closed change out of the local
  chain — abandon it, or fold it into a neighbour — and run `jj spr diff` over
  what is left; the new stack has a new number. Closing a pull request with
  nothing stacked on it moves no base, so it takes nothing apart at all and the
  stack it is in keeps its number.

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
