# Squint

A desktop app for reviewing GitHub pull requests across all your repositories,
built with Tauri 2 and Leptos (Rust on both sides).

- **Left:** a tree of repositories with the open and draft pull requests you are
  involved in (author, assignee, mentioned, commented, or review requested).
  Filter by *All / To review / Mine*, hide drafts, or type to search. `j` / `k`
  move through the list.
- **Middle, Diff tab:** the diff, unified or split, syntax highlighted, with
  word-level highlights. Lines with review comments get a bubble in the
  gutter. Lockfiles and very large files start folded.
- **Commenting on code:** hover a line in the diff and press its `+`; drag
  from the `+` to take in more lines of the same hunk. A comment box opens
  under the last line, and ⌘↩ posts it as a single comment. *Suggest* fills in
  a suggested change when the lines are all on the head side. A box you
  haven't sent survives folding, switching views and other pull requests, and
  a file header counts its drafts. GitHub only takes comments on lines the
  diff shows, and none while you have a review pending on github.com.
- **Middle, Code tab:** the whole repository at the PR's head, read-only (see
  below).
- **Middle, Logs tab:** a GitHub Actions job's log, opened from Checks (see
  below). Until one is open, it points you to Checks.
- **Right, Conversation tab:** description, comments, reviews and inline
  threads. Clicking a thread's path jumps to that line in the open tab.
  Threads can be replied to, resolved and unresolved, as far as GitHub
  allows you.
- **Right, Checks tab:** the workflow runs, other apps' checks and commit
  statuses on the PR's head, failing first.
- **Bottom, Review bar:** under the diff and the conversation. *Approve*
  approves the commit the diff shows (not one pushed since), and *Close PR*
  closes the pull request without merging, after a second click to confirm.
  Either one takes the optional comment: the review's body for *Approve*,
  posted right after closing for *Close PR*. A comment you haven't sent is
  kept per pull request while you look at others.
- **Side panes:** `⇤` and `⇥` fold the left and right panes into rails, to
  give the code more room. Drag a pane's inner edge to resize it; dragging it
  nearly shut folds it, and double-clicking the edge resets its width. A
  narrow window shrinks the panes before the diff.
- **Open in Zed** (the Z button): checks the pull request out and opens it in
  Zed. Each file header has its own Z button that opens that file at its
  first change. Buttons that show only an icon name themselves in a tooltip.

## The Code tab

A pared-down Zed for checking code the PR doesn't show: whether a method is
defined, where it's used, what a file around a change looks like.

- **Explorer:**
  - *Changed in this PR* is pinned above the file tree;
  - the tree badges changed files and folds single-child folders.
- **Viewer:**
  - code is syntax highlighted with tree-sitter, the parser Zed uses. Ruby,
    ERB, JS/TS, Python, Go, Rust, YAML, JSON, TOML, Bash, CSS, HTML and
    Markdown are covered;
  - lines the PR added get a green bar, and a red tick marks where it removed
    lines;
  - review threads show as bubbles;
  - *Show in diff* (±) and *Zed* (Z), at the line you're on, jump out.
- **Search:** `git grep` over the PR's head commit.
  - Toggle match case (`Aa`), whole word (`W`) and regex (`.*`).
  - Filter files with `*.rb, app/**, !spec/`; a name without a slash matches
    at any depth.
  - Results are grouped by file; `↑`/`↓` preview them, `Esc` clears.
- **⌘-click a name**, in the diff or the viewer: its definitions are listed
  first and the first one opens. Uses are listed below them.
- **From the diff:**
  - *View file* (the file icon) on a file header opens it at its first change;
  - clicking a line number opens that line in context. A removed line opens
    where it used to be.

| Key      | Action                                   |
| -------- | ---------------------------------------- |
| ⌘1 / ⌘2 / ⌘3 | Diff / Code / Logs tab               |
| ⌘P       | Go to file                               |
| ⌘⇧F      | Search the repo (for the selection, if any) |
| ⌘F       | Find in the open file, or search the log |
| ⌘-click  | Go to definition                         |

It reads the commit, not the worktree's files, so it shows the PR as it is
even after you edit the worktree in Zed. The diff's highlighting comes from
the same clone: each file is highlighted in full on both sides, so a hunk
that starts inside a string still colours correctly. Until the PR's worktree
is synced, the diff shows plain. Searches on a ~400-file repo take
about 20 ms. Files over 1 MB or 20,000 lines are left to Zed.

## Checks and the Logs tab

The Checks tab lists the PR's checks as GitHub counts them: its workflow runs
with their jobs (the latest run of each workflow, as a PR edit re-runs them),
then other apps' checks and commit statuses. Below them, *Other runs on this
commit* lists what GitHub doesn't count towards the PR, such as deployments
started by hand, newest first; they don't affect the summary or the tab's
colour. Clicking a job opens its log in the middle pane; any other check opens
its own page in the browser. While something is still running, the list and
the open run refresh every 15 seconds.

The log reads like GitHub's:

- one collapsible section per step, with its state and duration; failed
  steps start unfolded, scrolled to the first error;
- `##[group]` sections fold (nested ones too), `[command]` lines,
  `##[error]` and `##[warning]` stand out, and terminal colours are kept, so
  a Terraform plan's `+`, `-` and `~` read as they do in a terminal;
- **Search logs** (⌘F) unfolds the steps and groups with hits; `↵` / `⇧↵`
  step through them;
- a link to this repository's Actions in a comment, such as a bot's "read the
  full plan in Actions", opens here rather than in the browser. For a run
  the Checks tab doesn't list, a menu switches between its jobs.

GitHub only serves a job's log once the job has finished; until then, the
steps show live and the log is a click away on GitHub. The runner's log has
no step markers, so it is split by each step's start and end time and the
`Run …` header each step opens with. Logs over 32 MB are left to GitHub.

## Getting started

```sh
mise install                 # Rust 1.98 + wasm target, tauri-cli, trunk
mise run signing-identity    # once per Mac, see below
mise run dev                 # run with hot reload
```

Open **Settings** (⚙) and paste a GitHub token. A classic token needs the `repo`
and `read:org` scopes; if your organisation uses SAML single sign-on, authorise
the token for it. The token is checked against GitHub and stored in the macOS
Keychain (service `com.quintpieters.squint`, account `github-token`).
`GH_TOKEN` or `GITHUB_TOKEN` in the environment take precedence, which is handy
for a one-off dev run:

```sh
GITHUB_TOKEN=$(gh auth token) mise run dev
```

macOS asks for your login password the first time a build reads the token;
choose **Always Allow**. It remembers that per code signature, and a plain
build's ad-hoc signature changes every time you rebuild, so it would ask again
after each one. `mise run signing-identity` adds a self-signed certificate,
*Squint Local Signing*, to your login keychain, and both `mise run dev` and
`mise run build` sign with it (dev builds through Cargo's runner,
`scripts/run-signed.sh`). Every build then counts as the same app, and one
**Always Allow** lasts. Without the certificate, dev builds run unsigned and
`mise run build` fails.

## The review workspace

The app keeps its own fresh clones, so reviewing never touches the clones you
develop in:

```
~/.local/share/pr-reviews/github.com/<owner>/<repo>/
├── .bare/      blobless clone (the shared object store)
├── .git        "gitdir: ./.bare"
├── pr-2142/    worktree on the PR's own branch
└── pr-2160/
```

- **Freshness:** every inbox refresh (on start, every 5 minutes, ↻) fetches each
  repo once and moves each worktree to its PR's head. Force-pushes included:
  the app never runs `git pull`.
- **Selected PR first:** selecting a PR syncs it ahead of the queue, so
  **Open in Zed** opens `…/pr-<n>` right away.
- **Clean-up:** worktrees of merged or closed PRs are removed, and a repo goes
  with its last PR.
- **Your work is safe:**
  - a worktree with your changes is never moved or removed (`±` in the list);
  - **Stash & update** saves your work to the stash and a `backup/…` branch,
    then updates;
  - if a refresh fails, the last good worktree keeps working (`~`).
- **Location:** set `PR_REVIEWS_HOME` to put the workspace elsewhere. Settings
  shows the location, its disk usage, and any worktrees kept for your work.

The `zed` CLI is used when it is installed (Zed → *Install CLI*); otherwise the
app falls back to `open -a Zed`.

## Tasks

| Command         | What it does                                          |
| --------------- | ----------------------------------------------------- |
| `mise run dev`  | Run the app with hot reload                           |
| `mise run build`| Build the release `.app` and `.dmg`                   |
| `mise run check`| rustfmt, Clippy (native and wasm32), and the tests    |
| `mise run signing-identity` | Create the local code-signing certificate, once per Mac |
| `mise run icons` | Regenerate the app icons from `src-tauri/icons/icon.svg` |
| `mise run sandbox pr` | Open a test PR to review, see below            |

## Trying review actions

Comments, replies and approvals post to GitHub for real, so try them in a
private sandbox, `<your login>/squint-sandbox` (or `$SQUINT_SANDBOX`).
GitHub refuses to let you approve your own pull request, so the sandbox's
workflow opens them as github-actions[bot] and requests your review:

```sh
mise run sandbox pr           # a PR to review and approve, under To review
mise run sandbox pr --draft   # the same, as a draft
mise run sandbox own          # a PR by you: comments work, approving gets a 422
mise run sandbox reset        # close every open PR and delete its branch
```

Every PR makes the same change: three hunks in `lib/greeter.rb` (a changed
line, a removed line, a new method), a changed line in `src/main.rs` and a
new file. On the bot's PRs, the bot also starts a thread on each side of the
diff, to reply to and resolve.

## Layout

- `shared/`: types exchanged over IPC and the diff parser, used by both sides.
- `src/`: the Leptos UI, compiled to WebAssembly by Trunk.
- `src-tauri/`: the backend: GitHub API client, Keychain storage, the review
  workspace (`workspace.rs`, `sync.rs`), the Code tab's git reads and grep
  (`code.rs`), and Zed integration.
