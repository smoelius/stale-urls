# stale-urls

`stale-urls` checks commit-pinned GitHub source links in the current repository
for stale line references.

It uses `git ls-files` to select tracked files and recognizes URLs of this form:

```text
https://github.com/OWNER/REPOSITORY/blob/FULL_40_CHARACTER_COMMIT/PATH#L10-L20
```

## Usage

Install `stale-urls` from crates.io:

```console
cargo install stale-urls
```

Then run it from the repository to scan:

```console
cd /path/to/your/repository
stale-urls
```

Current URLs produce no individual report. For a confirmed stale URL,
`stale-urls` reports the best available change commit, the merge commit when
distinct, and every local occurrence of the URL.

The process exits unsuccessfully if any URL is stale or cannot be checked.
Interactive output uses color for status and report fields. Set `NO_COLOR` to
disable color; redirected output is always plain text.

## Workflow

`stale-urls` runs in four phases. When standard error is an interactive
terminal, each active phase displays a width-aware progress message that updates
in place instead of scrolling the terminal.

1. **Scanning** — `Scanning: PATH (i/n)`

   Uses `git ls-files` to read the tracked files in the current repository,
   collect URL occurrences, and deduplicate them. Tracked symlinks are read as
   their stored link values and are never followed. This phase is entirely
   local.

2. **Preparing** — `Preparing: OWNER/REPOSITORY (i/n)`

   Groups URLs by repository. Each required repository is shallow-cloned as a bare
   repository or updated by fetching its default branch in the user cache
   (`$XDG_CACHE_HOME/stale-urls`, normally
   `~/.cache/stale-urls`), and its missing pinned commits are fetched in a
   batch, with individual retries if the batch fails. Network access can occur
   during this phase.

3. **Checking** — `Checking: URL (i/n)`

   Uses only local Git objects to look for the exact, contiguous referenced
   lines anywhere in the same file on the repository's current default branch.
   Whitespace is significant. This phase performs no network access. Afterward,
   the tool prints `N URL(s) require investigation.`, including when `N` is
   zero.

4. **Investigating** — `Investigating: URL (i/n)`

   Examines URLs whose referenced lines were not found during checking. It
   fetches full default-branch history for the shallow repositories involved and
   follows ordinary file renames. If the lines remain unchanged in a renamed
   file, the URL is considered current; otherwise, it looks for the
   default-branch commit where they changed. For merged changes, it makes a
   best-effort attempt to find the original side-branch commit. Network access
   can occur during this phase.
