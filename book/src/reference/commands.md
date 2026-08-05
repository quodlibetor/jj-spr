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
- `-r, --revision <REV>` - Revision whose PR to close (default: `@`)

**Examples:**
```bash
# Close PR for current working copy
jj spr close

# Close PR for specific change
jj spr close -r <change-id>
```

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
