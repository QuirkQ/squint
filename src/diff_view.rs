use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use leptos::ev::MouseEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;
use shared::diff::split_rows;
use shared::{
    DiffLine, DiffShas, FileDiff, FileStatus, LineKind, PullDetail, PullSummary, Side, SyncStatus,
    TimelineItem,
};
use wasm_bindgen::JsCast;

use crate::api;
use crate::app::{Ctx, DiffMode, Load, Tab, line_id};
use crate::code_view::{CodePane, MarkKind, Marks, NO_TOKENS, Tokens, paint, tokens};
use crate::comments::{self, Composer, Pick, Placed, Span, Spot};
use crate::log_view::LogPane;
use crate::util::{self, Avatar, Icon};

/// Files this long start folded, as do the rest of a PR once this many lines
/// are already showing; rendering tens of thousands of rows makes the UI crawl.
const LARGE_FILE_LINES: usize = 600;
const EXPANDED_LINE_BUDGET: usize = 4000;

/// Generated files nobody reviews line by line.
fn is_generated(path: &str) -> bool {
    const LOCKFILES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "Gemfile.lock",
        "poetry.lock",
        "uv.lock",
        "composer.lock",
        "go.sum",
        "mix.lock",
        "flake.lock",
    ];
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCKFILES.contains(&name) || name.ends_with(".min.js") || name.ends_with(".snap")
}

pub fn default_collapsed(files: &[FileDiff]) -> HashSet<String> {
    let mut shown = 0;
    files
        .iter()
        .filter(|file| {
            let lines = file.line_count();
            let fold = is_generated(&file.path)
                || lines > LARGE_FILE_LINES
                || shown + lines > EXPANDED_LINE_BUDGET;
            if !fold {
                shown += lines;
            }
            fold
        })
        .map(|file| file.path.clone())
        .collect()
}

/// A comment thread anchored to a diff line.
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    /// Index into the timeline.
    pub thread: usize,
    pub comments: usize,
    pub resolved: bool,
}

pub type Markers = HashMap<String, HashMap<(Side, u32), Vec<Marker>>>;

pub fn markers(detail: &PullDetail) -> Markers {
    let mut markers = Markers::new();
    for (index, item) in detail.timeline.iter().enumerate() {
        if let TimelineItem::Thread(thread) = item
            && let Some(line) = thread.line
        {
            markers
                .entry(thread.path.clone())
                .or_default()
                .entry((thread.side, line))
                .or_default()
                .push(Marker {
                    thread: index,
                    comments: thread.comments.len(),
                    resolved: thread.is_resolved,
                });
        }
    }
    markers
}

#[component]
pub fn DiffPane() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <main class="diff-pane">
            {move || match ctx.selected.get() {
                None => {
                    view! {
                        <header class="pane-bar">
                            <span class="label">"Diff"</span>
                        </header>
                        <div class="empty-state">
                            <p>"Select a pull request on the left."</p>
                        </div>
                    }
                        .into_any()
                }
                Some(pr) => {
                    // Both tabs stay mounted, so switching keeps each one's
                    // scroll position and doesn't re-render a big diff.
                    view! {
                        <PullHeader pr=pr.clone()/>
                        <WorktreeNotice pr/>
                        <div class="diff-body" class:hidden=move || ctx.tab.get() != Tab::Diff>
                            <Files/>
                        </div>
                        <div class="code-body" class:hidden=move || ctx.tab.get() != Tab::Code>
                            <CodePane/>
                        </div>
                        <div class="logs-body" class:hidden=move || ctx.tab.get() != Tab::Logs>
                            <LogPane/>
                        </div>
                    }
                        .into_any()
                }
            }}
        </main>
    }
}

#[component]
fn PullHeader(pr: PullSummary) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let author = pr.author.clone().map(|a| {
        view! {
            <span class="author">
                <Avatar url=a.avatar_url size=16/>
                {a.login}
            </span>
        }
    });
    let mode_button = move |mode: DiffMode, icon: &'static str, title: &'static str| {
        view! {
            <button
                class:active=move || ctx.mode.get() == mode
                title=title
                on:click=move |_| ctx.mode.set(mode)
            >
                <Icon path=icon/>
            </button>
        }
    };
    let tab_button = move |tab: Tab, label: &'static str, title: &'static str| {
        view! {
            <button class:active=move || ctx.tab.get() == tab title=title on:click=move |_| ctx.tab.set(tab)>
                {label}
            </button>
        }
    };
    let on_diff = move || ctx.tab.get() == Tab::Diff;

    view! {
        <header class="pane-bar">
            <div class="tabs">
                {tab_button(Tab::Diff, "Diff", "The pull request's changes (⌘1)")}
                {tab_button(Tab::Code, "Code", "Browse and search the repository at the PR's head (⌘2)")}
                {tab_button(Tab::Logs, "Logs", "A job's log, opened from Checks (⌘3)")}
            </div>
            <span class="label crumb">{pr.repo.clone()} " #" {pr.number}</span>
            {pr.is_draft.then(|| view! { <span class="chip">"draft"</span> })}
            <span class="spacer"></span>
            <div class="segmented icons" class:hidden=move || !on_diff()>
                {mode_button(DiffMode::Unified, util::UNIFIED_ICON, "Unified diff")}
                {mode_button(DiffMode::Split, util::SPLIT_ICON, "Split diff, old and new side by side")}
            </div>
            // While busy, the worktree's progress shows beside the icon.
            <button
                class="btn primary"
                class:icon=move || !ctx.zed_busy.get()
                disabled=move || ctx.zed_busy.get()
                on:click=move |_| ctx.open_in_zed(None, None)
                title="Open this PR's worktree from the review workspace in Zed"
            >
                <Icon path=util::ZED_ICON/>
                {move || ctx.zed_progress()}
            </button>
            <a class="btn icon" href=pr.url.clone() title="Open on GitHub">
                <Icon path=util::GITHUB_ICON/>
            </a>
        </header>
        <div class="pr-heading" class:hidden=move || !on_diff()>
            <h2>{pr.title.clone()}</h2>
            <div class="pr-sub">
                {author}
                <span class="branches">
                    <code>{pr.head_ref.clone()}</code>
                    " → "
                    <code>{pr.base_ref.clone()}</code>
                </span>
                <span class="stats">
                    <span class="stat add">"+" {pr.additions}</span>
                    " "
                    <span class="stat del">"−" {pr.deletions}</span>
                </span>
            </div>
        </div>
    }
}

/// Explains a worktree that isn't simply up to date. The diff never depends
/// on it, so this is about what *Open in Zed* will show.
#[component]
fn WorktreeNotice(pr: PullSummary) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let number = pr.number;
    let repo = StoredValue::new(pr.repo);
    let update = move |_| {
        if !confirming.get_untracked() {
            confirming.set(true);
            set_timeout(
                move || confirming.set(false),
                std::time::Duration::from_secs(4),
            );
            return;
        }
        confirming.set(false);
        busy.set(true);
        let repo = repo.get_value();
        spawn_local(async move {
            match api::update_worktree(&repo, number).await {
                Ok(message) => ctx.notify(message, false),
                Err(error) => ctx.notify(error, true),
            }
            busy.set(false);
        });
    };

    move || {
        let notice = match repo.with_value(|repo| ctx.sync_status(repo, number))? {
            SyncStatus::LocalChanges => view! {
                <div class="worktree-notice">
                    <span class="glyph local">"±"</span>
                    <span class="text">
                        "Your changes in pr-" {number}
                        " keep its worktree on an older commit. The diff shows the PR as it is now."
                    </span>
                    <button
                        class="btn small"
                        class:primary=move || confirming.get()
                        disabled=move || busy.get()
                        on:click=update
                        title="Stash your changes (and keep any commits on a backup branch), then move the worktree to the PR's head"
                    >
                        {move || {
                            if busy.get() {
                                "Updating…"
                            } else if confirming.get() {
                                "Click again to stash & update"
                            } else {
                                "Stash & update"
                            }
                        }}
                    </button>
                </div>
            }
            .into_any(),
            SyncStatus::Stale { message } => view! {
                <div class="worktree-notice">
                    <span class="glyph stale">"~"</span>
                    <span class="text" title=message.clone()>
                        "Couldn't refresh the worktree, so Zed gets the last synced version. Retrying on the next refresh. "
                        <span class="detail">{message.clone()}</span>
                    </span>
                </div>
            }
            .into_any(),
            SyncStatus::Failed { message } => view! {
                <div class="worktree-notice">
                    <span class="glyph failed">"!"</span>
                    <span class="text" title=message.clone()>
                        "No worktree yet. Retrying on the next refresh. "
                        <span class="detail">{message.clone()}</span>
                    </span>
                </div>
            }
            .into_any(),
            _ => return None,
        };
        Some(notice)
    }
}

#[component]
fn Files() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let markers = Memo::new(move |_| {
        ctx.detail.with(|d| match d {
            Load::Ready(detail) => markers(detail),
            _ => Markers::new(),
        })
    });

    move || match ctx.files.get() {
        Load::Idle => ().into_any(),
        Load::Loading => view! { <div class="loading">"Loading diff…"</div> }.into_any(),
        Load::Failed(error) => view! { <div class="error-banner">{error}</div> }.into_any(),
        Load::Ready(files) => {
            let blocks = files
                .iter()
                .enumerate()
                .map(|(index, file)| view! { <FileBlock index file=file.clone() markers/> })
                .collect_view();
            view! {
                <FileList files=files.to_vec()/>
                <div class="files">{blocks}</div>
            }
            .into_any()
        }
    }
}

pub fn status_badge(status: FileStatus) -> (&'static str, &'static str) {
    match status {
        FileStatus::Added => ("status added", "A"),
        FileStatus::Removed => ("status removed", "D"),
        FileStatus::Renamed => ("status renamed", "R"),
        FileStatus::Copied => ("status renamed", "C"),
        _ => ("status modified", "M"),
    }
}

#[component]
fn FileList(files: Vec<FileDiff>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let all_paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let (additions, deletions) = files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let items = files
        .into_iter()
        .map(|file| {
            let (class, letter) = status_badge(file.status);
            let path = file.path.clone();
            view! {
                <li>
                    <button class="file-link" on:click=move |_| ctx.jump_to(&path, None)>
                        <span class=class>{letter}</span>
                        <span class="path">{file.path.clone()}</span>
                        <span class="stat add">"+" {file.additions}</span>
                        <span class="stat del">"−" {file.deletions}</span>
                    </button>
                </li>
            }
        })
        .collect_view();
    let count = all_paths.len();

    view! {
        <div class="file-summary">
            <details>
                <summary>
                    {count} {if count == 1 { " file changed" } else { " files changed" }}
                    <span class="stat add">" +" {additions}</span>
                    <span class="stat del">" −" {deletions}</span>
                </summary>
                <ul>{items}</ul>
            </details>
            <div class="fold-actions">
                <button
                    class="btn small icon"
                    title="Expand every file"
                    on:click=move |_| ctx.collapsed.set(HashSet::new())
                >
                    <Icon path=util::UNFOLD_ICON size=14/>
                </button>
                <button
                    class="btn small icon"
                    title="Collapse every file"
                    on:click=move |_| ctx.collapsed.set(all_paths.iter().cloned().collect())
                >
                    <Icon path=util::FOLD_ICON size=14/>
                </button>
            </div>
        </div>
    }
}

/// What a file's diff shows over its lines. Each part updates the cells it
/// touches, so a new comment never renders the whole file again, nor loses
/// what's typed in another box.
#[derive(Clone, Copy)]
struct Overlay {
    markers: Memo<HashMap<(Side, u32), Vec<Marker>>>,
    drafts: Memo<Vec<Placed>>,
    /// The lines being picked for a new comment, when it's in this file.
    picking: Memo<Option<Span>>,
}

impl Overlay {
    fn new(ctx: Ctx, file: &FileDiff, markers: Memo<Markers>) -> Self {
        let path = file.path.clone();
        let markers = {
            let path = path.clone();
            Memo::new(move |_| markers.with(|m| m.get(&path).cloned().unwrap_or_default()))
        };
        let drafts = {
            let file = file.clone();
            Memo::new(move |_| ctx.placed_drafts(&file))
        };
        let picking = Memo::new(move |_| {
            ctx.comments
                .pick
                .with(|pick| pick.as_ref().filter(|p| p.path == path).map(Pick::span))
        });
        Overlay {
            markers,
            drafts,
            picking,
        }
    }

    /// The line is picked, or has a comment being written on it.
    fn picked(&self, spot: Spot, column: Option<Side>) -> bool {
        self.picking
            .with(|p| p.is_some_and(|span| span.covers(spot, column)))
            || self.drafts.with(|drafts| {
                drafts
                    .iter()
                    .any(|d| d.span.is_some_and(|span| span.covers(spot, column)))
            })
    }
}

#[component]
fn FileBlock(index: usize, file: FileDiff, markers: Memo<Markers>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let path = file.path.clone();
    let folded = {
        let path = path.clone();
        Memo::new(move |_| ctx.collapsed.with(|c| c.contains(&path)))
    };
    let syntax = load_syntax(ctx, &file, folded);
    let overlay = Overlay::new(ctx, &file, markers);
    let toggle = {
        let path = path.clone();
        move |_| {
            ctx.collapsed.update(|c| {
                if !c.remove(&path) {
                    c.insert(path.clone());
                }
            })
        }
    };
    let open_file = {
        let path = path.clone();
        let line = file.first_changed_line();
        move |_| ctx.open_in_zed(Some(path.clone()), line)
    };
    let view_file = {
        let path = path.clone();
        let line = file.first_changed_line();
        move |_| ctx.view_code(path.clone(), line)
    };
    let threads = move || {
        overlay
            .markers
            .with(|lines| lines.values().map(Vec::len).sum::<usize>())
    };
    let drafts = move || overlay.drafts.with(Vec::len);
    let (status_class, letter) = status_badge(file.status);
    let title = match &file.previous_path {
        Some(previous) => format!("{previous} → {}", file.path),
        None => file.path.clone(),
    };
    let removed = file.status == FileStatus::Removed;
    let (additions, deletions) = (file.additions, file.deletions);

    view! {
        <section class="file" id=format!("file-{index}")>
            <header class="file-head">
                <button class="file-toggle" on:click=toggle>
                    <span class="chevron" class:open=move || !folded.get()>"▸"</span>
                    <span class=status_class>{letter}</span>
                    <span class="path" title=title.clone()>{title.clone()}</span>
                </button>
                {move || {
                    let count = drafts();
                    (count > 0)
                        .then(|| {
                            view! {
                                <span class="chip draft-chip" title="Comments you're writing here and haven't sent">
                                    {count} {if count == 1 { " draft" } else { " drafts" }}
                                </span>
                            }
                        })
                }}
                {move || {
                    let count = threads();
                    (count > 0).then(|| view! { <span class="chip">{count} " 💬"</span> })
                }}
                <span class="stat add">"+" {additions}</span>
                <span class="stat del">"−" {deletions}</span>
                <button
                    class="btn small icon"
                    disabled=removed
                    on:click=view_file
                    title="Read the whole file at the PR's head in the Code tab"
                >
                    <Icon path=util::FILE_CODE_ICON size=14/>
                </button>
                <button
                    class="btn small icon"
                    disabled=move || removed || ctx.zed_busy.get()
                    on:click=open_file
                    title="Open this file in Zed"
                >
                    <Icon path=util::ZED_ICON size=14/>
                </button>
            </header>
            {move || {
                (!folded.get())
                    .then(|| file_body(ctx, index, &file, overlay, ctx.mode.get(), syntax.get()))
            }}
        </section>
    }
}

/// Syntax highlighting for a file's diff, by side and line number.
#[derive(Debug, Default, PartialEq)]
struct DiffSyntax {
    old: HashMap<u32, Tokens>,
    new: HashMap<u32, Tokens>,
}

impl DiffSyntax {
    fn tokens(&self, line: &DiffLine) -> &Tokens {
        let found = match line.kind {
            LineKind::Del => line.old.and_then(|n| self.old.get(&n)),
            _ => line.new.and_then(|n| self.new.get(&n)),
        };
        found.unwrap_or(&NO_TOKENS)
    }
}

/// Highlights a file's diff from both full versions of the file, which is
/// right even where a hunk starts inside a string or a comment. It loads
/// when the file is first unfolded, once the worktree is synced; until then
/// the diff shows plain.
fn load_syntax(ctx: Ctx, file: &FileDiff, folded: Memo<bool>) -> RwSignal<Option<Arc<DiffSyntax>>> {
    let syntax = RwSignal::new(None);
    let loaded_for = StoredValue::new(None::<DiffShas>);
    let (old_ranges, new_ranges) = file.line_ranges();
    let old_path = (file.status != FileStatus::Added).then(|| {
        file.previous_path
            .clone()
            .unwrap_or_else(|| file.path.clone())
    });
    let new_path = (file.status != FileStatus::Removed).then(|| file.path.clone());
    if file.patch_missing || file.hunks.is_empty() {
        return syntax;
    }
    Effect::new(move |_| {
        if folded.get() {
            return;
        }
        let Load::Ready(shas) = ctx.diff_shas.get() else {
            return;
        };
        if loaded_for.with_value(|l| l.as_ref() == Some(&shas)) {
            return;
        }
        loaded_for.set_value(Some(shas.clone()));
        let Some(repo) = ctx
            .selected
            .with_untracked(|pr| pr.as_ref().map(|pr| pr.repo.clone()))
        else {
            return;
        };
        let (old_path, new_path) = (old_path.clone(), new_path.clone());
        let (old_ranges, new_ranges) = (old_ranges.clone(), new_ranges.clone());
        spawn_local(async move {
            let side = |sha: String, path: Option<String>, ranges: Vec<(u32, u32)>| {
                let repo = repo.clone();
                async move {
                    let Some(path) = path.filter(|_| !ranges.is_empty()) else {
                        return HashMap::new();
                    };
                    api::highlight_lines(&repo, &sha, &path, &ranges)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(line, spans)| (line, tokens(&spans)))
                        .collect()
                }
            };
            let old = side(shas.base.clone(), old_path, old_ranges).await;
            let new = side(shas.head.clone(), new_path, new_ranges).await;
            if loaded_for.with_value(|l| l.as_ref() == Some(&shas)) {
                syntax.set(Some(Arc::new(DiffSyntax { old, new })));
            }
        });
    });
    syntax
}

fn file_body(
    ctx: Ctx,
    index: usize,
    file: &FileDiff,
    overlay: Overlay,
    mode: DiffMode,
    syntax: Option<Arc<DiffSyntax>>,
) -> AnyView {
    let syntax = syntax.unwrap_or_default();
    if file.patch_missing {
        return view! {
            <p class="file-note">"No diff available: binary file or too large to show."</p>
        }
        .into_any();
    }
    if file.hunks.is_empty() {
        let note = if file.status == FileStatus::Renamed {
            "File renamed without changes."
        } else {
            "No content changes."
        };
        return view! { <p class="file-note">{note}</p> }.into_any();
    }

    let gutter = move |side: Side, line: Option<u32>| {
        let line = line?;
        overlay.markers.with(|markers| {
            let found = markers.get(&(side, line))?;
            let comments: usize = found.iter().map(|m| m.comments).sum();
            let resolved = found.iter().all(|m| m.resolved);
            let thread = found[0].thread;
            Some(view! {
                <button
                    class="bubble"
                    class:resolved=resolved
                    title=format!("{comments} comment(s)")
                    on:click=move |ev| {
                        ev.stop_propagation();
                        ctx.show_thread(thread);
                    }
                >
                    {comments}
                </button>
            })
        })
    };
    // A line number opens that line, in full context, in the Code tab; a
    // removed line opens the place it was removed from.
    let path = StoredValue::new(file.path.clone());
    let has_head = file.status != FileStatus::Removed;
    // `pick` is the split view's: the line and column the cell belongs to.
    let num_cell = move |side: Side,
                         line: Option<u32>,
                         class: &'static str,
                         head: Option<u32>,
                         pick: Option<(Spot, Side)>| {
        let head = head.filter(|_| has_head && line.is_some());
        view! {
            <td
                class=class
                class:to-code=head.is_some()
                class:picked=move || pick.is_some_and(|(spot, column)| overlay.picked(spot, Some(column)))
                id=line.map(|n| line_id(index, side, n))
                data-spot=pick.map(|(spot, _)| spot.attr())
                data-col=pick.map(|(_, column)| column_attr(column))
                on:click=move |_| {
                    if let Some(head) = head {
                        ctx.view_code(path.get_value(), Some(head));
                    }
                }
            >
                {move || gutter(side, line)}
                {line}
            </td>
        }
    };
    // Pressing + starts picking lines; dragging takes in more, and letting
    // go opens the comment box.
    let add_button = move |spot: Spot, column: Option<Side>| {
        view! {
            <button
                class="add-comment"
                tabindex="-1"
                title="Comment on this line; drag to comment on several"
                on:mousedown=move |ev: MouseEvent| {
                    if ev.button() == 0 {
                        ev.prevent_default();
                        ctx.start_pick(path.get_value(), column, spot);
                    }
                }
            >
                "+"
            </button>
        }
    };
    let on_hover = move |ev: MouseEvent| {
        if ctx.comments.pick.with_untracked(Option::is_none) {
            return;
        }
        let cell = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest("[data-spot]").ok().flatten());
        let Some(cell) = cell else {
            return;
        };
        let Some(spot) = cell
            .get_attribute("data-spot")
            .and_then(|a| Spot::parse(&a))
        else {
            return;
        };
        let column = match cell.get_attribute("data-col").as_deref() {
            Some("old") => Some(Side::Old),
            Some("new") => Some(Side::New),
            _ => None,
        };
        path.with_value(|path| ctx.extend_pick(path, column, spot));
    };

    let rows = file.hunks.iter().enumerate().map(|(h, hunk)| {
        let header = match mode {
            DiffMode::Unified => view! {
                <tr class="hunk">
                    <td class="num" colspan="2"></td>
                    <td class="code">{hunk.header.clone()}</td>
                </tr>
            }
            .into_any(),
            DiffMode::Split => view! {
                <tr class="hunk">
                    <td class="code" colspan="4">{hunk.header.clone()}</td>
                </tr>
            }
            .into_any(),
        };
        let lines = match mode {
            DiffMode::Unified => hunk
                .lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    let spot = Spot { hunk: h, index: i };
                    let class = line_class(line.kind);
                    let head = hunk.head_line(i);
                    view! {
                        <tr class=class class:picked=move || overlay.picked(spot, None) data-spot=spot.attr()>
                            {num_cell(Side::Old, line.old, "num", head, None)}
                            {num_cell(Side::New, line.new, "num", head, None)}
                            <td class="code">
                                {add_button(spot, None)}
                                {sign(line.kind)}
                                {code(line, syntax.tokens(line))}
                            </td>
                        </tr>
                        {draft_rows(ctx, overlay.drafts, mode, move |after| after == spot)}
                    }
                })
                .collect_view()
                .into_any(),
            DiffMode::Split => split_rows(&hunk.lines)
                .into_iter()
                .map(|row| {
                    let old = row.old.map(|i| &hunk.lines[i]);
                    let new = row.new.map(|i| &hunk.lines[i]);
                    let side_class = |line: Option<&DiffLine>| match line.map(|l| l.kind) {
                        Some(LineKind::Del) => "del",
                        Some(LineKind::Add) => "add",
                        Some(LineKind::Context) => "",
                        None => "empty",
                    };
                    let (old_class, new_class) = (side_class(old), side_class(new));
                    let old_head = row.old.and_then(|i| hunk.head_line(i));
                    let new_head = row.new.and_then(|i| hunk.head_line(i));
                    let at = |index: Option<usize>, column: Side| {
                        index.map(|index| (Spot { hunk: h, index }, column))
                    };
                    let (old_pick, new_pick) = (at(row.old, Side::Old), at(row.new, Side::New));
                    let picked = move |pick: Option<(Spot, Side)>| {
                        pick.is_some_and(|(spot, column)| overlay.picked(spot, Some(column)))
                    };
                    let cell = |line: Option<&DiffLine>, pick: Option<(Spot, Side)>| {
                        let (line, (spot, column)) = (line?, pick?);
                        Some(view! {
                            {add_button(spot, Some(column))}
                            {sign(line.kind)}
                            {code(line, syntax.tokens(line))}
                        })
                    };
                    let here = move |after: Spot| {
                        after.hunk == h && (Some(after.index) == row.old || Some(after.index) == row.new)
                    };
                    view! {
                        <tr class="line">
                            {num_cell(Side::Old, old.and_then(|l| l.old), num_class(old_class), old_head, old_pick)}
                            <td
                                class=format!("code {old_class}")
                                class:picked=move || picked(old_pick)
                                data-spot=old_pick.map(|(spot, _)| spot.attr())
                                data-col="old"
                            >
                                {cell(old, old_pick)}
                            </td>
                            {num_cell(Side::New, new.and_then(|l| l.new), num_class(new_class), new_head, new_pick)}
                            <td
                                class=format!("code {new_class}")
                                class:picked=move || picked(new_pick)
                                data-spot=new_pick.map(|(spot, _)| spot.attr())
                                data-col="new"
                            >
                                {cell(new, new_pick)}
                            </td>
                        </tr>
                        {draft_rows(ctx, overlay.drafts, mode, here)}
                    }
                })
                .collect_view()
                .into_any(),
        };
        view! {
            {header}
            {lines}
        }
    });

    let columns = match mode {
        DiffMode::Unified => view! {
            <colgroup>
                <col class="c-num"/>
                <col class="c-num"/>
                <col/>
            </colgroup>
        }
        .into_any(),
        DiffMode::Split => view! {
            <colgroup>
                <col class="c-num"/>
                <col/>
                <col class="c-num"/>
                <col/>
            </colgroup>
        }
        .into_any(),
    };
    let table_class = match mode {
        DiffMode::Unified => "diff unified",
        DiffMode::Split => "diff split",
    };
    view! {
        <div class="diff-scroll">
            <table class=table_class class:picking=move || overlay.picking.with(Option::is_some) on:mouseover=on_hover>
                {columns}
                <tbody>{rows.collect_view()}</tbody>
            </table>
        </div>
    }
    .into_any()
}

fn column_attr(column: Side) -> &'static str {
    match column {
        Side::Old => "old",
        Side::New => "new",
    }
}

/// The comment boxes that follow a row: those of drafts whose last line
/// `here` says is in it. Keyed, so opening another box leaves these be.
fn draft_rows(
    ctx: Ctx,
    drafts: Memo<Vec<Placed>>,
    mode: DiffMode,
    here: impl Fn(Spot) -> bool + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <For
            each=move || {
                drafts
                    .with(|d| d.iter().filter(|d| here(d.after)).map(|d| d.id).collect::<Vec<_>>())
            }
            key=|id| *id
            children=move |id| draft_row(ctx, drafts, id, mode)
        />
    }
}

fn draft_row(ctx: Ctx, drafts: Memo<Vec<Placed>>, id: u32, mode: DiffMode) -> AnyView {
    let Some(draft) = ctx.comments.draft(id) else {
        return ().into_any();
    };
    let placed = drafts.with_untracked(|d| d.iter().find(|d| d.id == id).copied());
    let gone = placed.is_some_and(|p| p.span.is_none());
    let side = placed.map_or(draft.end.side, |p| p.end_side);
    let composer = view! {
        <div class="draft">
            <div class="draft-head">
                {draft.heading()}
                {gone.then(|| view! { <span class="muted">" · no longer in this diff"</span> })}
            </div>
            <Composer
                id=comments::draft_box_id(id)
                text=draft.text.clone()
                placeholder="Leave a comment"
                send_label="Comment"
                on_send=Callback::new(move |()| ctx.post_draft(id))
                on_cancel=Callback::new(move |()| ctx.discard_draft(id))
                suggestion=draft.suggestion.clone()
            />
        </div>
    };
    match (mode, side) {
        (DiffMode::Unified, _) => view! {
            <tr class="draft-row">
                <td class="draft-gap" colspan="2"></td>
                <td class="draft-cell">{composer}</td>
            </tr>
        }
        .into_any(),
        (DiffMode::Split, Side::Old) => view! {
            <tr class="draft-row">
                <td class="draft-cell old" colspan="2">{composer}</td>
                <td class="draft-gap" colspan="2"></td>
            </tr>
        }
        .into_any(),
        (DiffMode::Split, Side::New) => view! {
            <tr class="draft-row">
                <td class="draft-gap old" colspan="2"></td>
                <td class="draft-cell" colspan="2">{composer}</td>
            </tr>
        }
        .into_any(),
    }
}

fn num_class(side_class: &str) -> &'static str {
    match side_class {
        "del" => "num del",
        "add" => "num add",
        "empty" => "num empty",
        _ => "num",
    }
}

fn line_class(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Add => "line add",
        LineKind::Del => "line del",
        LineKind::Context => "line",
    }
}

fn sign(kind: LineKind) -> impl IntoView {
    let sign = match kind {
        LineKind::Add => "+",
        LineKind::Del => "-",
        LineKind::Context => " ",
    };
    view! { <span class="sign">{sign}</span> }
}

/// The line's text, syntax highlighted, with word-level changes wrapped in
/// `<mark>`.
fn code(line: &DiffLine, tokens: &Tokens) -> impl IntoView + use<> {
    let emphasis: Marks = line
        .emphasis
        .iter()
        .map(|&(start, end)| (start, end, MarkKind::Emphasis))
        .collect();
    let parts = paint(&line.text, tokens, &emphasis);
    let no_newline = line
        .no_newline
        .then(|| view! { <span class="no-newline" title="No newline at end of file">"⏎̸"</span> });
    view! {
        {parts}
        {no_newline}
    }
}
