# Plan: a review workspace and a Code tab

## Goal

1. **A review workspace the app owns.** It keeps fresh clones of every
   repository in your inbox under `~/.local/share/pr-reviews`, and a ready
   worktree for every open and draft pull request, on that PR's branch.
   Reviewing never touches the clones you develop in: no fetches into them, no
   registered worktrees, no branch switches.
2. **A Code tab** next to the diff. It works like a pared-down Zed: browse the
   tree at the PR's head, read any file (changed or not), and `git grep` the
   whole repo to check quickly whether a method is defined and where it is used.
   Because the worktree is already synced, this is instant.

It is read-only on purpose; **Open in Zed** opens the same worktree for editing.

## Decided

- A worktree per PR, keyed by PR number, on the PR's own branch. A PR's branches
  can change; per-PR directories cost a little disk and buy a lower error rate.
- Every open and draft PR in the inbox is synced. There is no scope setting.
- What is gone gets cleaned up immediately: the worktree of a merged, closed or
  no-longer-yours PR, and a repo once its last PR is gone. A PR is either open,
  draft or gone; there are no grace periods and no made-up "idle" state.

## 1. The review workspace

### Layout

```
~/.local/share/pr-reviews/                  # $PR_REVIEWS_HOME, else $XDG_DATA_HOME/pr-reviews
└── github.com/
    └── acme/
        └── shop/
            ├── .bare/                      # bare, blobless clone: the repo's single object store
            ├── .git                        # "gitdir: ./.bare", so `git -C shop …` just works
            ├── pr-2142/                    # worktree on branch feat/guest-checkout
            └── pr-2160/                    # worktree on branch renovate/foo
```

- **Per-PR directories** are named `pr-<number>`: stable across branch renames,
  unique, and what Zed shows as the project name.
- **The bare-repo-plus-sibling-worktrees layout** shares one object store across
  all of a repo's worktrees, so fifty PRs on `acme/shop` cost fifty checkouts,
  not fifty clones.
- **There is no index file.** The directory tree is the source of truth (a few
  dozen `pr-*` directories is instant to scan), so nothing can drift out of
  sync. Live sync status is kept in memory.

### Clones

- `git clone --bare --filter=blob:none --no-tags https://github.com/<owner>/<repo>.git .bare`.
  This fetches all commits and trees but only the file contents that get checked
  out, which keeps big repos like `rails/rails` (in your inbox) affordable.
- **Fixing up the bare clone.** A bare clone mirrors every branch into
  `refs/heads`, and git refuses to update a branch that is checked out in a
  worktree. So after cloning:
  - the fetch refspec becomes `+refs/heads/*:refs/remotes/origin/*`;
  - the mirrored `refs/heads/*` are deleted, so the branch list inside a worktree
    (and Zed's branch picker) only shows the worktrees' own branches.
- Authentication is the token from Settings, via the `GIT_CONFIG_*` extra-header
  environment the app already uses. No SSH keys or credential helpers are
  involved, and the token is never written to disk.

### A worktree per PR, on the PR's branch

- **Same-repo PR:** a local branch named like the PR's head branch (e.g.
  `feat/guest-checkout`), with upstream `origin/<head>`. Zed then shows the
  real branch name, and you can commit and push a small fix-up from it if you
  want to.
- **Fork PR, or a name already checked out by another worktree:** branch
  `pr/<number>`, with no upstream.
- **Commit:** always the PR's head sha, fetched as
  `+refs/pull/<n>/head:refs/pull/<n>/head` (this works for forks too, and GitHub's
  layout can't clash with a local branch such as `pr/<n>`).
- **Branch changes:** if the head branch is renamed, the next sync renames the
  local branch. If the base is retargeted, the new base branch is fetched.

### Keeping it fresh

- **When.** Every inbox refresh (on start, every 5 minutes, and ↻) starts a sync
  round. A refresh that arrives mid-round is coalesced into one follow-up round
  with the newest inbox.
- **Selected PR first.** Selecting a PR calls `ensure_pr`, which syncs just that
  PR straight away under the repo's lock. When the round reaches it, there's
  nothing left to do.
- **One fetch per repo.** It covers every PR in the inbox for that repo, their
  head branches (same-repo PRs) and their base branches, for example
  `git fetch origin +refs/pull/2142/head:refs/pull/2142/head +refs/heads/main:refs/remotes/origin/main …`.
  If a branch vanished mid-round (a PR merged with its branch deleted), it
  retries with only the `refs/pull` refspecs.
- **Updating a worktree** when the head moved, including force-pushes, uses
  `git checkout -B <branch> <sha>`. It only does this when nothing of yours would
  be lost:
  - there are no uncommitted changes;
  - `HEAD` is either the head the app last synced (`refs/pull/<n>/head`,
    read just before the fetch) or an ancestor of the new head.

  Otherwise the worktree is left alone and flagged *local changes*. Your work is
  never discarded, and no state file is needed to decide.
- **Concurrency.** A per-repo lock serialises git operations on the same
  `.bare`. A semaphore runs three repos at a time. Order: the selected PR's repo,
  then repos with *To review* PRs, then *Mine*, then the rest.
- **Status in the UI.** Status reaches the UI as Tauri events, with no polling.
  - A status bar at the foot of the sidebar shows `SYNC 12/76` during a
    round, and how many failed.
  - Each PR row gets a mono glyph: `·` queued, `◌` syncing, nothing when ready,
    `±` local changes, `~` stale (last good worktree in use), `!` failed
    (hover for the git error).
  - Open in Zed (and later the Code tab) shows the live step: "cloning
    acme/shop…", "checking out pr-2142…".

### When things go wrong

The review view never depends on a sync succeeding. Each case below is covered
by a test against a local stand-in for GitHub.

- **The diff and conversation** come from the GitHub API, not from git.
- **Force-pushes** are routine: the app never runs `git pull`. It fetches with
  forced refspecs (`+…`) and moves worktrees with `checkout -B`.
- **Your changes hold a worktree back.** The worktree stays as it is, but the
  PR head's files are made local anyway, in one batched fetch, so the Code tab
  reads the PR as it is now. Without that batch, git fetches each missing file
  separately: 18 files took 10.5s that way, against 0.8s for one batch.
  **Stash & update** saves your work, then moves the worktree: uncommitted
  changes to the stash, commits that are nowhere else to a
  `backup/pr-<n>-<sha>` branch.
- **A fetch fails.**
  - Lock files a crashed git left behind (older than 5 minutes) are cleared,
    and the fetch is retried once after 2 seconds.
  - If it still fails, the PR is marked *stale*: its last synced worktree stays
    usable, Open in Zed opens it with a warning, and every round retries.
- **A worktree breaks** (a deleted admin directory, a half-finished checkout):
  it is rebuilt. If git can't even tell whether it holds your work, it is moved
  to `.broken/` instead of deleted.
- **`refs/pull` lags a push by a moment.** The app records the commit it
  actually checked out, so the next round catches up.

### Clean-up: gone means gone

There are only two states for a PR: in your inbox (open or draft) or gone
(merged, closed, or you're no longer involved).

- **After every complete inbox fetch,** worktrees of PRs that are not in it are
  removed (`git worktree remove`, plus the local branch and the PR's ref).
- **A repo whose last PR is gone** has its whole directory removed.
- **"Complete"** means GitHub answered without errors and without hitting the
  pagination cap. A partial answer (an SSO-protected org, a timeout) never
  deletes anything.
- **The one exception is your own work.** A worktree with uncommitted changes,
  untracked files, or commits that are on neither GitHub nor the PR's head is
  kept. Settings lists it with *Delete anyway*.

### Scope and disk

Every open and draft PR gets a worktree. The first sync of your current inbox
(76 PRs, 27 repos) is the expensive moment. After that, syncs are incremental
fetches plus resets, and clean-up keeps the disk bounded to the PRs that are
actually open.

### Settings

The workspace location (with *Reveal in Finder*), its disk usage, *Sync now*,
and any kept-back worktrees.

### What it replaced (done)

Open in Zed used to borrow objects from a clone it found under `~/code` and keep
worktrees in the app's data folder. That code is gone (`find_clone`,
`PR_REVIEW_CODE_DIRS`). Open in Zed is now "make sure this PR is synced
(`ensure_pr`), then `zed <worktree> [file:line]`". There was nothing to migrate.

## 2. The Code tab

The middle pane's top bar gets two tabs. *Unified / Split* only shows on Diff.

```
┌ acme/checkout #1170   [DIFF][CODE]                     [Open in Zed] [GitHub ↗] ┐
├────────────────────────────┬─────────────────────────────────────────────────────┤
│ SEARCH                     │ lib/active_record/tenanted/base.rb      @ 7e14a25   │
│ [def tenanted           ]  │                        [Show in diff] [Zed :13]     │
│ [Aa] [W] [.*]  in [*.rb ]  ├────┬────────────────────────────────────────────────┤
├────────────────────────────┤ 11 │   module Base                                  │
│ CHANGED IN THIS PR    [3]  │ 12 │     class_methods do                           │
│  M  config/database.yml    │ 13 ▌       def tenanted(config_name = "primary")   │
│ FILES          ⌘P go to    │ 14 ▌         self.tenanted_config_name = …         │
│ ▾ app/                     │ 15 │       end                                      │
│   ▾ models/                │ 16 │ [2]   def tenanted?                            │
│      account.rb            │    │                                                │
│ ▸ config/                  │    │                                                │
└────────────────────────────┴────┴────────────────────────────────────────────────┘
```

- **Explorer (left, ~260px):** a search box on top. Below it, either the file
  tree (with a pinned *Changed in this PR* list) or the search results, grouped
  by file, when a search is active.
- **Viewer (right):** the file with line numbers. A green bar marks lines this
  PR added; a red tick marks where it deleted lines. Review-thread bubbles
  (`[2]`) work as they do in the diff. Matches from a search are highlighted,
  and the target line is centred and flashed.
- The conversation pane can be collapsed (remembered), so the Code tab gets the
  width.

### Reading from the worktree

Every query addresses the PR's head commit inside its worktree. Measured on this
machine:

| Need          | Git                                                                      | Measured                          |
| ------------- | ------------------------------------------------------------------------ | --------------------------------- |
| File list     | `git ls-tree -r -z --name-only <sha>`                                    | 24ms for 269 files                |
| Read a file   | `git show <sha>:<path>`                                                  | instant                           |
| Search        | `git grep -n --column -I -z --full-name -F\|-E\|-P [-i] [-w] -e <q> <sha> -- [pathspec]` | 15–20ms (`acme/shop`, activerecord-tenanted) |

Grepping `<sha>` rather than the files on disk keeps results exact even after you
edit the worktree in Zed. Checking out the worktree is what makes the head
commit's file contents local in a blobless clone. I verified this with
`GIT_NO_LAZY_FETCH=1`: grep at the head succeeded, and at an un-checked-out base
it failed. Base-side search therefore needs a checkout of its own (phase 4).

### Entry points

- **From the Diff tab:**
  - each file header gets **View file**, which opens the Code tab at the first
    change;
  - clicking a diff line number opens that line in the Code tab, with the full
    surrounding context;
  - **⌘-click an identifier** in the diff or the viewer finds its definition: a
    definition search first, then the plain word matches as "references". Use
    WebKit's `caretRangeFromPoint` to get the word under the pointer;
  - selecting text and pressing ⌘⇧F searches for the selection.
- **From the viewer:** *Show in diff* (when the file is part of the PR) and *Zed
  :line*.
- **Keyboard:**

  | Key      | Action                              |
  | -------- | ----------------------------------- |
  | ⌘1 / ⌘2  | Diff / Code tab                     |
  | ⌘P       | go to file                          |
  | ⌘⇧F      | search                              |
  | ⌘F       | find in the open file               |
  | Esc      | close the palette / clear search    |

  `j`/`k` keep stepping through PRs when you aren't typing.

### Grep details

- **Output format.** Parse `<sha>:<path>\0<line>\0<column>\0<text>`. Exit code 1
  means "no matches", not an error.
- **Streaming.** Read stdout incrementally and stop git after 2,000 matches; trim
  long lines to ~300 bytes around the match.
- **Regex flavour.** Regex mode uses PCRE (`-P`, which Apple's git has). If git
  says it was built without it, the app remembers that and uses `-E`, which
  lacks `\b`/`\s`.
- **Definition search** (done differently than first planned). A symbol search
  runs one fixed-string, whole-word, case-sensitive grep (up to 20,000 lines).
  The `regex` crate then sorts those lines into definitions and uses. It
  recognises:
  - `def | fn | function | func | class | module | struct | enum | trait |
    interface | type | const | let | var | val | static | alias …`, with
    `self.`/`Foo::` receivers, Go's `(s *Server)` receivers and Ruby's `?`/`!`
    suffixes;
  - Ruby/Rails macros (`attr_reader`, `has_one`, `scope`, `delegate`,
    `let(:x)` …);
  - constants assigned at the start of a line.

  Asking git for the definition patterns themselves was too slow. On
  dependabot-core (7,600 files), POSIX regexes took 5.9 s and PCRE hit its
  backtracking limit; one fixed grep plus the regex crate takes 0.17 s. It also
  no longer depends on how git was built.
- **Highlight ranges.** The `regex` crate computes them; when it can't parse a
  pattern git accepted (PCRE look-arounds), git's column marks the first match.
- **Path filter.** `*.rb, app/**, !vendor/` becomes `:(glob)` / `:(exclude,glob)`
  pathspecs. A name without a slash matches at any depth (`**/*.rb`). A plain
  name such as `models` also matches everything inside a directory of that
  name. A filter of only exclusions searches everything else. The filter is
  remembered.
- **Safety.** The pattern always goes after `-e`. Paths are relative with
  `Normal` components only, and `sha` must be hex. The UI never sends a
  filesystem path; the backend resolves `repo` to its clone inside the
  workspace.

### Measured (done)

| Repo                                  | Files | Tree  | Text grep  | Symbol (definitions + uses) |
| ------------------------------------- | ----- | ----- | ---------- | --------------------------- |
| `acme/shop` #2142                     | 415   | 24 ms | 19–25 ms   | 16–30 ms                    |
| `basecamp/activerecord-tenanted` #283 | 269   | 23 ms | 14 ms      | 15 ms (`tenanted`: 1 + 263) |
| dependabot-core (local copy)          | 7,614 | 29 ms | 80–190 ms  | 170 ms                      |

In the real app, ⌘-click to listed results takes 39 ms. The viewer renders a
2,000-line file in 65 ms with ⌘F at 22–29 ms per keystroke. At the
20,000-line cap it takes 0.4–0.6 s to render and 0.25–0.4 s per ⌘F
keystroke, which is what virtualised rendering (phase 4) is for.

## Backend (`src-tauri/`)

| Module          | Role                                                                                         |
| --------------- | -------------------------------------------------------------------------------------------- |
| `git.rs`        | the authenticated `git` command builder and runner, moved out of `zed.rs`                    |
| `workspace.rs`  | layout, clone, refspecs, worktree create/update, clean-up                                    |
| `sync.rs`       | rounds, coalescing, per-repo locks, the three-repo semaphore, status events                  |
| `code.rs`       | `tree`, `read_file`, `grep` with symbol search (done)                                         |
| `zed.rs`        | `launch(worktree, path, line)` only                                                          |

- **Commands:** `workspace_status`, `workspace_disk_usage`, `ensure_pr`,
  `update_worktree` (*Stash & update*), `remove_worktree` (*Delete anyway*),
  `reveal_workspace`, and `open_in_zed`. The last now takes
  only `repo, number, path, line`; the backend knows the head from the inbox.
  Phases 2–3 added `code_tree(repo, number)` (the head sha and its paths),
  `read_file(repo, sha, path)` and `grep(repo, sha, query)`.
- **Inbox data:** `PullSummary` gains `head_sha` and `is_fork` (`headRefOid`,
  `isCrossRepository`), and `Inbox` gains `complete`. After every successful
  `inbox`, the backend hands the PR list to the sync engine.

## Shared crate (`shared/`)

- **Types:** `SyncStatus { Queued, Syncing { step }, Ready, LocalChanges, Stale { message }, Failed { message } }`,
  `SyncEvent`, `WorkspaceSummary`; `CodeTree`, `FileContent` (`Text`, `Binary`,
  `TooLarge`), `GrepQuery`, `GrepMatch`, `GrepResult`.
- **Pure, unit-tested helpers:**
  - `worktree_branch(pr, taken)` (head name, or `pr/<n>` for forks and
    collisions);
  - `build_tree(paths)` (with Zed-style folding of single-child directories),
    `fuzzy_score(query, path)` for ⌘P, `word_at(text, offset)` for ⌘-click,
    and `is_symbol`;
  - on the diff, `FileDiff::head_changes()` for the viewer's gutter and
    `Hunk::head_line(i)`, so a removed line opens where it was removed from.

  Grep parsing, pathspecs, definition matching and excerpts are
  backend-only, in `code.rs`.

## Phases

Each phase ends usable on its own.

1. **Review workspace** (L, **done**). `git.rs`, `workspace.rs`, `sync.rs`; the inbox
   triggers syncs and clean-up; sidebar status glyphs; Open in Zed rebuilt on
   top; local-clone discovery deleted; the workspace section in Settings.
   *Done when*:
   - a fresh start syncs your whole inbox into `~/.local/share/pr-reviews`;
   - Zed opens `…/acme/shop/pr-2142` on its branch;
   - a force-push to a PR updates its worktree on the next refresh;
   - a merged PR's worktree disappears.
2. **Browse** (M, **done**). Tabs (both stay mounted, so switching keeps
   scroll positions); `code_tree`, `read_file`; tree with *Changed in this PR*
   and change badges; the viewer with the changed-line gutter, removed-line
   ticks and thread bubbles; *View file*; clickable diff line numbers; the
   collapsible conversation pane (remembered). Thread links open the Code
   tab's line when it's the active tab.
3. **Grep** (M, **done**). Search with case / whole-word / regex toggles and a
   path filter; grouped results with ↑/↓ preview; open-at-match; ⌘-click
   definitions (in the diff and the viewer) that open the first definition;
   ⌘⇧F (with the selection); ⌘F. "Is `shadow_expiry` defined?" is answered in
   one gesture.
4. **Polish** (M). Done:
   - **The ⌘P fuzzy finder** (PR files rank higher).
   - **Syntax highlighting** with tree-sitter (see below), in the viewer and
     the diff.

   Left:
   - **Virtualised rendering** for files near the 20,000-line cap.
   - **A base/head toggle,** using an on-demand `base-<branch>` worktree.

## Syntax highlighting (done)

- **Engine.** `tree-sitter-highlight` 0.27 with the grammars' own queries
  (`highlight.rs`).
  - **Languages:** Ruby (including `Gemfile`, `Rakefile`, `.rake`,
    `.jbuilder`), ERB (with Ruby and HTML injected), Rust, JavaScript/JSX,
    TypeScript/TSX, Python, Go, YAML, JSON, TOML, Bash, CSS, HTML and
    Markdown (fenced code injected).
  - **Not covered:** HAML/Slim and Dockerfiles show plain.
- **Output.** Spans are flat `[start, end, kind]` byte triples, cut at line
  breaks. `kind` indexes `shared::code::HIGHLIGHTS`, which maps capture names
  to a dozen CSS classes themed with `--syn-*` variables (light and dark).
  Plain variables, operators and most punctuation stay unstyled.
- **Viewer.** `read_file` returns the spans with the text (off the async
  threads). Each line is cut at both token and search/find boundaries, so
  marks keep the colours underneath.
- **Diff.** It's highlighted from both *full* versions of each file, not the
  patch fragments, which would go wrong wherever a hunk starts inside a
  string, heredoc or comment.
  - `diff_shas(repo, number, base, paths)` finds the head and its merge base,
    and fetches the old versions of the changed files in one batch. A
    blobless clone doesn't have them, and fetching them one at a time is
    slow.
  - `highlight_lines(repo, sha, path, ranges)` returns spans for just the
    lines the hunks show.
  - A file loads its highlights when it's first unfolded. The word-level
    `<mark>`s keep their syntax colours.
  - Until the worktree is synced, the diff shows plain.
- **Measured.**
  - All 414 files of PR #2142 highlight in 213 ms in total.
  - In the app, the diff's colours arrive about 0.35 s after the diff itself.
  - 32 of its 35 files get colour; the rest (`Procfile`, `.env`) have no
    grammar.
- **Cost.** The release binary went from 5.3 MB to 16.4 MB, almost all of it
  the grammars' parse tables (TypeScript/TSX and Ruby are the biggest).
  Dropping languages nobody reviews is the way to trim it.
- **Workarounds.**
  - **Pattern order:** later patterns win in `tree-sitter-highlight`, so
    TypeScript's queries go after JavaScript's.
  - **Markdown inline text:** injecting the inline grammar loses most of its
    highlights inside `tree-sitter-highlight`. So each inline run outside
    headings is highlighted on its own and merged in.

## Testing

- **shared:** unit tests for every helper, including the `worktree_branch`
  collision and fork cases.
- **workspace and sync:** tests against a local "remote". `git init --bare` a
  repo in a temp dir, create `refs/pull/N/head` refs by pushing to them, and
  point the workspace at it with a `file://` base URL. Cover:
  - clone, then worktree on the right branch (same-repo and fork);
  - head moves → updated;
  - force-push → reset;
  - dirty worktree → left alone and flagged;
  - local commit → left alone;
  - head branch renamed → local branch renamed;
  - PR gone after a complete inbox → removed, and the repo once its last PR goes;
  - PR gone but dirty → kept;
  - partial inbox → nothing removed;
  - two syncs racing on one repo → one worktree.

  No network is needed.
- **code (phases 2–3, done):** list, read and grep against the same fixture:
  - binary file, big file, path with spaces, no-match exit code, truncation,
    `../` rejected;
  - case, whole-word, regex, path filters and directory names, and regex
    errors;
  - definitions for Ruby `?`/`!` methods, `attr_reader`, constants and
    classes.

  The UI was checked twice:
  - in headless Edge with a mock backend serving PR #2142's real files
    (screenshots, light and dark);
  - in the real Tauri app (WebKit) against the inbox: tree, View file, search,
    path filter, PCRE, ⌘-click in viewer and diff, ⌘P, binary files, ⌘F.
- **live and end to end:**
  - a probe syncing a few real PRs into a scratch `PR_REVIEWS_HOME`;
  - the real-Tauri hook against your inbox;
  - headless-Edge screenshots of the sync glyphs and Settings, later the Code
    tab, in light and dark.

## Risks and limits

- **First sync cost.** 27 clones plus 76 checkouts takes minutes and gigabytes on
  a big inbox. It runs in the background, with the selected PR first.
- **Re-cloning.** When a repo's last PR closes, its clone goes too, so the next
  PR there clones again. The clones are blobless, so this is cheap.
- **Pushing from review worktrees.** It is possible on same-repo PRs. Pushing uses
  your normal git credentials (the app's token is only passed to its own fetches).
- **What can't be shown:** Git LFS files show their pointer; submodules have no
  content; files over 1MB / 20k lines show a notice and *Open in Zed*.
- **"Definition" is a regex heuristic.** It is fast and dependency-free, but can
  miss metaprogrammed methods (`define_method`, `attr_accessor`). The references
  list underneath is the safety net.

## Open questions

1. **Search exclusions** (answered: no defaults). Results match what `git grep`
   gives in a terminal. The path filter (`!vendor/, !*.lock`) is one field
   away and is remembered.
2. **Syntax highlighting** (answered): tree-sitter, done for the viewer and the
   diff.
