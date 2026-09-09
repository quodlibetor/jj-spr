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

Land (squash-merge) an approved pull request.

**Usage:**
```bash
jj spr land [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision to land (default: `@`)
- `--cherry-pick` - Land PR independently (for use with stacks)

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

---

### `jj spr list`

List open pull requests and their status.

**Usage:**
```bash
jj spr list [OPTIONS]
```

**Options:**
- `--format <FORMAT>` - How to print the listing: `table` (the default),
  `slack`, or `slack-links`. Taken from the `spr.listFormat` setting when the
  flag is not given.
- `--copy` - Also put the listing on the clipboard, as rich text where the
  format has links to carry.

**Output includes:**
- Title and URL
- Review status (approved, changes requested, etc.)
- CI status
- Whether the conversation is waiting on your reply. Reacting to a comment
  counts as answering it, so a thumbs-up settles a thread you have nothing to
  add to.

Pull requests are listed in the order of the local changes that carry them,
newest change first, so a stack reads the way `jj log` prints it. Separate
stacks are listed one after another, and a `Stack` column marks where each one
starts and ends when there is more than one. A pull request that no local
change carries — landed elsewhere, or opened from another machine — is listed
last.

**Asking for reviews:** `--format slack` prints the listing as a Markdown
bullet list to paste into a chat message — one bullet per pull request, its
title under an emoji for where the review stands (⏳ pending, 💬 commented on,
🔴 changes requested, ✅ accepted), and its URL on the line below:

```
- ⏳ Add the widget cache
  https://github.com/acme/codez/pull/12
- 💬 Rework the parser
  https://github.com/acme/codez/pull/11
```

`--format slack-links` prints the same list with each title made a terminal
hyperlink to its pull request instead of the URL being printed underneath, so
every pull request is one short line. Terminals that do not understand the
escape show the title alone, and so, at the time of writing, does a paste into
Slack — which is what the plain `slack` format is for.

Both formats separate stacks with a blank line, and leave out the merge and
comment columns: what a reviewer needs from the message is which pull requests
are waiting on them.

**Getting the links into Slack:** no terminal turns a hyperlink back into a
link when you copy it — every copy path writes plain text, so the URL behind a
`slack-links` title is dropped on the way to the clipboard. `--copy` goes
around that by putting the listing on the clipboard itself, as HTML with a
real link per pull request alongside a plain-text version. Pasting that into
Slack gives live links, the way pasting from a browser does. Both chat formats
put the same HTML there — a linked title is what each was reaching for — and
both fall back to the same spelled-out plain text, since the escapes that draw
a hyperlink on screen paste as rubbish anywhere else.

`--copy` works on the machine jj-spr runs on, so it does nothing useful over
SSH. On X11 and Wayland the clipboard belongs to the process that set it, so
what is copied outlives `jj spr list` only where a clipboard manager is
running — most desktops run one.

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
