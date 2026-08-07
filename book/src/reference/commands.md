# Commands Reference

This page provides a complete reference for all SPR commands.

## Global Options

All commands support the following global options:

- `-h, --help` - Show help information
- `-V, --version` - Show version information

## Commands

### `jj spr init`

Initialize SPR in the current repository. This command prompts for your GitHub Personal Access Token and configures the repository.

**Usage:**
```bash
jj spr init
```

**What it does:**
- Detects GitHub repository from git remotes
- Prompts for GitHub Personal Access Token
- Stores configuration in git config

---

### `jj spr diff`

Create or update a pull request for one or more changes.

**Usage:**
```bash
jj spr diff [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision(s) to operate on (default: `@-`)
  - Single revision: `-r @-`, `-r <change-id>`
  - Range: `-r main..@`, `-r a::c`
- `-a, --all` - Create/update PRs for all changes from base to current
- `--base <REV>` - Base revision for `--all` mode (default: trunk)
- `-m, --message <MSG>` - Message for PR update commits
- `--update-message` - Update PR title/description from local commit
- `--draft` - Create PR as draft
- `--cherry-pick` - Create PR as if cherry-picked onto main

**Examples:**
```bash
# Create PR for parent of working copy (default)
jj spr diff

# Create PR for specific change
jj spr diff -r <change-id>

# Create PRs for all changes in range
jj spr diff -r main..@

# Create independent PR
jj spr diff --cherry-pick

# Update PR with new changes and message
jj spr diff -m "Address review comments"
```

---

### `jj spr land`

Land an approved pull request: squash-merge it, or put it in the merge queue of
the default branch where that branch has one.

**Usage:**
```bash
jj spr land [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision to land (default: `@`)
- `--cherry-pick` - Land PR independently (for use with stacks)
- `--queue` - Put the PR in the merge queue, whatever `spr.landStrategy` says
- `--no-queue` - Squash-merge the PR now, whatever `spr.landStrategy` says
- `--wait` - Stay until the merge queue has merged the PR, then clean up after it

**Examples:**
```bash
# Land PR for parent of working copy
jj spr land -r @-

# Land specific change
jj spr land -r <change-id>

# Land independently (with --cherry-pick)
jj spr land --cherry-pick -r <change-id>
```

**Stacks:** Landing a pull request lands every pull request below it that has
not landed, bottom first, and then that one. A pull request branch carries the
local stack under it, so merging one from the middle of a stack on its own would
put all of those changes on the default branch inside a single squash under a
single title, and leave their pull requests open with nothing left to show.
Landing them in their own right gives one commit per pull request, and closes
each one with the merge that carried it — the same thing GitHub's own
`gh stack merge` does with a stack. jj-spr says which pull requests a land is
going to merge before it starts.

A change below the one you asked for that has no pull request refuses the land:
passing over it would not leave it unlanded — the branch above carries it — it
would only land it with nothing on GitHub to say so. Run `jj spr diff` over the
stack first. A pull request pushed with `--cherry-pick` carries its change onto
the default branch by itself, so landing one lands nothing below it.

Landing a pull request also retargets the pull requests stacked on top of
it at the default branch, so the rest of the stack is ready to land without
another `jj spr diff`. The base branches they pointed at are deleted, except
under [`spr.baseStrategy = linear`](configuration.md#basestrategy), where what
they pointed at is the landed pull request's own branch and taking it away
would close them. Until you rebase and run `jj spr diff` again, though, a
retargeted pull request's diff on GitHub still includes the changes that just
landed.

**GitHub stacks:** With
[`spr.stackDisplay = github`](configuration.md#stackdisplay), landing takes the
GitHub stack holding the pull request apart before it merges anything, and then
merges that pull request on its own. It has to: GitHub refuses to merge a pull
request a stack holds, and offers its own stack merge instead. Run `jj spr diff`
afterwards to register what is left as a stack again; it gets a new number and a
new URL, and any pull request that was in the dissolved stack but is not in the
run you push is left unstacked — jj-spr says which.

Taking a stack apart cannot be undone, so landing asks GitHub whether it will
merge before it dissolves anything, wherever it can. Landing the bottom pull
request of a stack moves no base branch, so that answer is the same before the
stack comes apart as after it, and a land GitHub turns down — a required check
still running, a conflict, an unmet requirement — leaves the stack standing.
Higher up the stack the base has to move onto the default branch first, and
moving it is what needs the stack gone, so there a refusal still costs the
stack.

Landing deliberately does *not* take GitHub up on that stack merge
(`PUT /pulls/{n}/merge-async`), and not because of what it lands: merging
everything below the pull request you asked for is right, and `jj spr land`
does it too. The reason is what GitHub does afterwards. It rebases the head
branch of the pull request *above* onto its new base, and the branches jj-spr
pushes are merge commits that a rebase discards: the branch collapses onto its
base and GitHub closes the pull request as having no changes, review and all.
For the same reason, do not merge a stacked pull request from GitHub's own
interface.

**Important:** After landing, you must manually rebase your working copy:
```bash
jj git fetch
jj rebase -r @ -d main@origin
```

#### Merge queues

A branch with a merge queue takes no merge that does not go through the queue,
so `jj spr land` asks GitHub whether the default branch has one and queues the
pull request where it does. Nothing needs configuring for that;
[`spr.landStrategy`](./configuration.md) is for saying so explicitly, or for
merging directly as somebody entitled to bypass the queue.

A queued land ends as soon as the pull request is in the queue. GitHub merges it
later, so unlike a squash-merge, `jj spr land` does not delete the pull request
branches — the queue merges the pull request branch, and deleting it would take
the pull request out of the queue. Fetch and rebase once GitHub has merged it,
and use `jj spr cleanup --confirm` to remove the branches left behind, which it
sees as orphans once GitHub has closed the pull request.

`--wait` does that for you instead: it stays until GitHub has merged the pull
request, then deletes the branches and fetches what landed, the way a
squash-merging land does. It waits for as long as the queue takes, and stopping
it leaves the pull request queued.

Landing a pull request with unlanded ones below it needs `--wait` where the
default branch has a queue, and is refused without it. Each pull request has to
be merged before the next can be queued, so a land that will not wait cannot
reach the second one — it is refused up front rather than part way up the
stack.

```bash
# Queue the PR and return
jj spr land -r <change-id>

# Queue the PR and stay until the queue has merged it
jj spr land --wait -r <change-id>
```

A merge queue exists for the default branch and takes a pull request based on
it, so a stacked pull request is retargeted at the default branch before it is
queued, exactly as it is before a squash-merge. Where the local change has
parents that have not landed, that means the queue merges those parents'
commits too, and `jj spr land` warns before it queues. Land the pull requests
below it first to avoid that.

---

### `jj spr list`

List open pull requests and their status.

**Usage:**
```bash
jj spr list
```

**Output includes:**
- PR number and title
- Current state (open, draft, etc.)
- Review status (approved, changes requested, etc.)
- CI status

---

### `jj spr close`

Close a pull request without merging.

**Usage:**
```bash
jj spr close [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision, or revision range, whose PRs to close
  (default: `@-`)
- `-a, --all` - Close the PRs of every commit from the base to the revision
- `--base <REV>` - Base revision for `--all` (default: trunk)

**Examples:**
```bash
# Close PR for the parent of the working copy
jj spr close

# Close PR for specific change
jj spr close -r <change-id>
```

**Stacks:** Closing a pull request retargets the pull requests based on it at
*its* base, not at the default branch — closing puts nothing on the default
branch, so sending them there would pull in the changes of every pull request
below, which are still under review. Each retargeted pull request's diff does
grow to include the closed pull request's changes, which is reported when it
happens: those changes are no longer under review anywhere else.

The closed pull request's branch is only deleted once every pull request based
on it has been moved, because GitHub closes a pull request whose base branch
disappears. If one of them cannot be moved, the branch is kept and said so.
Its base branch is deleted only where jj-spr generated that branch for this
pull request alone, and only while nothing has been pointed at it. So a base
branch you set yourself is never deleted, and neither is the one a stacked pull
request has under [`spr.baseStrategy =
linear`](configuration.md#basestrategy), which is the branch of the pull
request below. Where `linear` fell back to generating a base branch after all,
that branch *is* deleted — unless some open pull request still targets it,
which the pull requests just retargeted onto it are the usual reason for.

**GitHub stacks:** With
[`spr.stackDisplay = github`](configuration.md#stackdisplay), the close itself
needs nothing special: GitHub allows a stacked pull request to be closed, and
keeps it in the stack in place with the stack still open. What a stack refuses
is the retargeting that follows, because a stack owns its members' base refs.
So closing takes apart whatever stack holds each pull request it has to move —
usually the single stack they were all in, but a pull request based on the
closed one's branch that jj-spr never pushed drags its stack in too. Run
`jj spr diff` afterwards to register what is left as a stack again; it gets a
new number and a new URL, and any pull request that was in the dissolved stack
but is not in the run you push is left unstacked — jj-spr says which, not
counting the one it just closed.

Take the closed change out of the local chain before that `jj spr diff` —
abandon it, or fold it into a neighbour. Closing takes the pull request number
off the change but leaves the change where it was, so a run that pushes it
opens a *new* pull request rather than putting the old one back. Merely
skipping it in the revset does not work either: jj-spr does not step over a
change, it builds around it. It chains a change onto the one below only when
that one is its local parent, so whatever sat on the closed change gets a base
branch of its own carrying that change's work — the chain breaks across the
gap, and the changes you closed drop back out of the diffs above, which is the
opposite of what the retargeting just reported.

Closing a pull request with nothing stacked on it moves no base, so it takes
no stack apart and the stack keeps its number. The closed pull request stays in
it, which is what GitHub does with a closed member; there is no way to remove
one pull request from a stack short of destroying the stack for every other
member.

---

### `jj spr amend`

Update local commit message with content from GitHub PR.

**Usage:**
```bash
jj spr amend [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision to update (default: `@`)

**Use case:** When PR title/description has been updated on GitHub and you want to sync those changes back to your local commit.

---

## Revision Syntax

SPR supports Jujutsu's revision syntax:

- `@` - Current working copy
- `@-` - Parent of working copy
- `<change-id>` - Specific change by ID (e.g., `qpvuntsm`)
- `main@origin` - Remote tracking branch
- `main..@` - Range from main to current
- `a::c` - Inclusive range from a to c

See [Jujutsu revset documentation](https://martinvonz.github.io/jj/latest/revsets/) for more details.
