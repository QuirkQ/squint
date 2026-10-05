//! The Code tab: a read-only explorer and viewer for the repository at the
//! pull request's head, with `git grep` search, ⌘P and ⌘-click to definition.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use leptos::ev::KeyboardEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;
use shared::code::{HIGHLIGHTS, ancestors, build_tree, fuzzy_score};
use shared::{
    FileBody, FileContent, FileStatus, GrepMatch, GrepQuery, GrepResult, HeadChanges, Side,
    SyncStatus, TreeNode,
};

use crate::api;
use crate::app::{Ctx, Load, Tab};
use crate::diff_view::{markers, status_badge};
use crate::util::{self, Icon};

const PATHS_KEY: &str = "code-search-paths";
/// Typing pauses this long before a search runs.
const SEARCH_DELAY: Duration = Duration::from_millis(150);
const PALETTE_ROWS: usize = 50;

/// A pull request's files, with the explorer tree built once.
#[derive(Debug, PartialEq)]
pub struct Loaded {
    pub repo: String,
    pub number: u32,
    pub sha: String,
    pub paths: Vec<String>,
    pub nodes: Arc<[TreeNode]>,
}

/// A file as the viewer shows it.
#[derive(Debug, PartialEq)]
pub struct OpenFile {
    pub content: FileContent,
    /// The text's lines, without line endings.
    pub lines: Vec<String>,
    /// Each line's syntax highlighting.
    pub tokens: Vec<Tokens>,
}

impl OpenFile {
    fn new(content: FileContent) -> Self {
        let FileBody::Text(text) = &content.body else {
            return OpenFile {
                content,
                lines: Vec::new(),
                tokens: Vec::new(),
            };
        };
        let mut spans = content.highlights.as_chunks::<3>().0.iter().peekable();
        let (mut lines, mut tokens) = (Vec::new(), Vec::new());
        let mut offset = 0;
        for raw in text.strip_suffix('\n').unwrap_or(text).split('\n') {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let (start, end) = (offset as u32, (offset + line.len()) as u32);
            let mut line_tokens = Tokens::new();
            // Spans arrive in order and never cross a line break.
            while let Some(span) = spans.next_if(|span| span[0] < start + raw.len() as u32) {
                let (from, to) = (span[0].max(start), span[1].min(end));
                if let (true, Some((_, class))) = (from < to, HIGHLIGHTS.get(span[2] as usize)) {
                    line_tokens.push((from - start, to - start, *class));
                }
            }
            lines.push(line.to_string());
            tokens.push(line_tokens);
            offset += raw.len() + 1;
        }
        OpenFile {
            content,
            lines,
            tokens,
        }
    }
}

/// Syntax highlighting of one line: byte ranges and their CSS class.
pub type Tokens = Vec<(u32, u32, &'static str)>;

pub static NO_TOKENS: Tokens = Vec::new();

/// A line's highlights as the backend sends them, as [`Tokens`].
pub fn tokens(spans: &[u32]) -> Tokens {
    spans
        .as_chunks::<3>()
        .0
        .iter()
        .filter_map(|&[start, end, kind]| Some((start, end, HIGHLIGHTS.get(kind as usize)?.1)))
        .collect()
}

/// Where the viewer should go.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub path: String,
    pub line: Option<u32>,
    /// Bumped on every request, so going to the same place again still
    /// scrolls and flashes.
    pub nonce: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub query: GrepQuery,
    pub result: Result<Arc<GrepResult>, String>,
    pub millis: u32,
}

#[derive(Clone, Copy)]
pub struct CodeState {
    pub tree: RwSignal<Load<Arc<Loaded>>>,
    pub file: RwSignal<Load<Arc<OpenFile>>>,
    pub target: RwSignal<Option<Target>>,
    /// The line *Zed* opens at: the target's, or the line number last clicked.
    pub cursor: RwSignal<Option<u32>>,
    pub search: RwSignal<GrepQuery>,
    pub outcome: RwSignal<Option<Outcome>>,
    pub searching: RwSignal<bool>,
    /// The highlighted search result, for ↑/↓.
    pub active: RwSignal<usize>,
    pub expanded: RwSignal<HashSet<String>>,
    pub palette: RwSignal<bool>,
    pub find_open: RwSignal<bool>,
    pub find_query: RwSignal<String>,
    pub find_case: RwSignal<bool>,
    pub find_index: RwSignal<usize>,
    /// The Code tab was opened, so its files are worth loading.
    visited: RwSignal<bool>,
    /// The next symbol search result opens its first definition.
    auto_open: StoredValue<bool>,
}

impl Default for CodeState {
    fn default() -> Self {
        CodeState {
            tree: RwSignal::new(Load::Idle),
            file: RwSignal::new(Load::Idle),
            target: RwSignal::new(None),
            cursor: RwSignal::new(None),
            search: RwSignal::new(GrepQuery {
                paths: util::load(PATHS_KEY).unwrap_or_default(),
                ..GrepQuery::default()
            }),
            outcome: RwSignal::new(None),
            searching: RwSignal::new(false),
            active: RwSignal::new(0),
            expanded: RwSignal::new(HashSet::new()),
            palette: RwSignal::new(false),
            find_open: RwSignal::new(false),
            find_query: RwSignal::new(String::new()),
            find_case: RwSignal::new(false),
            find_index: RwSignal::new(0),
            visited: RwSignal::new(false),
            auto_open: StoredValue::new(false),
        }
    }
}

impl CodeState {
    /// Every listed search result, in order, as `(path, line)`.
    fn listed(&self) -> Vec<(String, u32)> {
        self.outcome.with(|outcome| match outcome {
            Some(Outcome {
                result: Ok(result), ..
            }) => result
                .definitions
                .iter()
                .chain(&result.matches)
                .map(|m| (m.path.clone(), m.line))
                .collect(),
            _ => Vec::new(),
        })
    }
}

impl Ctx {
    /// Opens a file in the Code tab, at a line if given.
    pub fn view_code(&self, path: String, line: Option<u32>) {
        let code = self.code;
        self.tab.set(Tab::Code);
        code.expanded
            .update(|e| e.extend(ancestors(&path).map(str::to_string)));
        let nonce = code
            .target
            .with_untracked(|t| t.as_ref().map_or(0, |t| t.nonce + 1));
        code.cursor.set(line);
        let id = tree_id(&path);
        code.target.set(Some(Target { path, line, nonce }));
        set_timeout(move || util::scroll_nearest(&id), Duration::from_millis(30));
    }

    /// ⌘-click: where `symbol` is defined, and where it's used.
    pub fn find_symbol(&self, symbol: String) {
        let code = self.code;
        self.tab.set(Tab::Code);
        code.auto_open.set_value(true);
        code.search.update(|q| {
            q.pattern = symbol;
            q.symbol = true;
            q.case_sensitive = true;
            q.whole_word = true;
            q.regex = false;
        });
    }

    /// ⌘⇧F: searches for `text`, or just focuses the search box.
    pub fn search_code(&self, text: Option<String>) {
        let code = self.code;
        self.tab.set(Tab::Code);
        if let Some(text) = text {
            code.search.update(|q| {
                q.pattern = text;
                q.symbol = false;
            });
        }
        set_timeout(|| util::focus("code-search"), Duration::from_millis(30));
    }

    /// ⌘F: finds in the open file.
    pub fn find_in_file(&self) {
        let code = self.code;
        if code.file.with_untracked(|f| matches!(f, Load::Ready(_))) {
            code.find_open.set(true);
            set_timeout(|| util::focus("find-input"), Duration::from_millis(30));
        }
    }
}

/// Like the app's other loaders, but a value may stay on screen while its
/// replacement loads, when `keep` says so: the tree while the same PR
/// refreshes, a file until the next one is read.
pub fn load_latest<K, T, F, Fut>(
    key: Memo<Option<K>>,
    target: RwSignal<Load<T>>,
    keep: impl Fn(&T, &K) -> bool + 'static,
    fetch: F,
) where
    K: Clone + PartialEq + Send + Sync + 'static,
    T: Send + Sync + 'static,
    F: Fn(K) -> Fut + 'static,
    Fut: Future<Output = Result<T, String>> + 'static,
{
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        let Some(key) = key.get() else {
            target.set(Load::Idle);
            return;
        };
        generation.update_value(|g| *g += 1);
        let current = generation.get_value();
        let stale_is_fine = target.with_untracked(|t| matches!(t, Load::Ready(v) if keep(v, &key)));
        if !stale_is_fine {
            target.set(Load::Loading);
        }
        let request = fetch(key);
        spawn_local(async move {
            let result = request.await;
            if generation.get_value() == current {
                target.set(match result {
                    Ok(value) => Load::Ready(value),
                    Err(error) => Load::Failed(error),
                });
            }
        });
    });
}

/// Loads the selected pull request's files once the Code tab is used, the
/// open file, and search results.
pub fn setup(ctx: Ctx, key: Memo<Option<(String, u32)>>) {
    let code = ctx.code;

    // A new pull request starts with a fresh explorer.
    Effect::new(move |previous: Option<Option<(String, u32)>>| {
        let current = key.get();
        if previous.is_some_and(|previous| previous != current) {
            code.target.set(None);
            code.cursor.set(None);
            code.outcome.set(None);
            code.expanded.set(HashSet::new());
            code.find_open.set(false);
            code.palette.set(false);
        }
        current
    });
    Effect::new(move |_| {
        if ctx.tab.get() == Tab::Code {
            code.visited.set(true);
        }
    });
    Effect::new(move |_| util::store(PATHS_KEY, &code.search.with(|q| q.paths.clone())));

    // Reload the files whenever the PR's worktree has (re)synced.
    let ready = Memo::new(move |_| {
        let status = ctx.selected.with(|pr| {
            pr.as_ref()
                .and_then(|pr| ctx.sync_status(&pr.repo, pr.number))
        });
        !matches!(
            status,
            Some(SyncStatus::Queued | SyncStatus::Syncing { .. } | SyncStatus::Failed { .. })
        )
    });
    let epoch = RwSignal::new(0u32);
    Effect::new(move |previous: Option<bool>| {
        let now = ready.get();
        if now && previous == Some(false) {
            epoch.update(|e| *e += 1);
        }
        now
    });
    let tree_key = Memo::new(move |_| {
        if !code.visited.get() {
            return None;
        }
        let (repo, number) = key.get()?;
        Some((repo, number, epoch.get()))
    });
    load_latest(
        tree_key,
        code.tree,
        |loaded, (repo, number, _)| loaded.repo == *repo && loaded.number == *number,
        |(repo, number, _)| async move {
            let tree = api::code_tree(&repo, number).await?;
            let nodes = build_tree(&tree.paths);
            Ok(Arc::new(Loaded {
                repo,
                number,
                sha: tree.sha,
                paths: tree.paths,
                nodes,
            }))
        },
    );

    // The diff's highlighting needs its two commits, and the old versions of
    // its files, which the backend fetches in one go.
    let diff_key = Memo::new(move |_| {
        let (repo, number, base) = ctx.selected.with(|pr| {
            pr.as_ref()
                .map(|pr| (pr.repo.clone(), pr.number, pr.base_ref.clone()))
        })?;
        let paths = ctx.files.with(|files| match files {
            Load::Ready(files) => Some(
                files
                    .iter()
                    .filter(|f| f.status != FileStatus::Added)
                    .map(|f| f.previous_path.clone().unwrap_or_else(|| f.path.clone()))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })?;
        Some((repo, number, base, paths, epoch.get()))
    });
    load_latest(
        diff_key,
        ctx.diff_shas,
        |_, _| false,
        |(repo, number, base, paths, _)| async move {
            api::diff_shas(&repo, number, &base, &paths).await
        },
    );

    let file_key = Memo::new(move |_| {
        let path = code.target.with(|t| t.as_ref().map(|t| t.path.clone()))?;
        code.tree.with(|tree| match tree {
            Load::Ready(tree) => Some((tree.repo.clone(), tree.sha.clone(), path)),
            _ => None,
        })
    });
    load_latest(
        file_key,
        code.file,
        |_, _| true,
        |(repo, sha, path)| async move {
            let content = api::read_file(&repo, &sha, &path).await?;
            Ok(Arc::new(OpenFile::new(content)))
        },
    );

    let search_key = Memo::new(move |_| {
        let query = code.search.get();
        if query.pattern.trim().is_empty() {
            return None;
        }
        code.tree.with(|tree| match tree {
            Load::Ready(tree) => Some((tree.repo.clone(), tree.sha.clone(), query)),
            _ => None,
        })
    });
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        generation.update_value(|g| *g += 1);
        let current = generation.get_value();
        let Some((repo, sha, query)) = search_key.get() else {
            code.outcome.set(None);
            code.searching.set(false);
            return;
        };
        code.searching.set(true);
        let delay = if query.symbol {
            Duration::ZERO
        } else {
            SEARCH_DELAY
        };
        set_timeout(
            move || {
                if generation.get_value() != current {
                    return;
                }
                spawn_local(async move {
                    let started = js_sys::Date::now();
                    let result = api::grep(&repo, &sha, &query).await.map(Arc::new);
                    if generation.get_value() != current {
                        return;
                    }
                    let millis = (js_sys::Date::now() - started) as u32;
                    if query.symbol
                        && code.auto_open.get_value()
                        && let Ok(found) = &result
                        && let Some(first) = found.definitions.first()
                    {
                        ctx.view_code(first.path.clone(), Some(first.line));
                    }
                    code.auto_open.set_value(false);
                    code.active.set(0);
                    code.searching.set(false);
                    code.outcome.set(Some(Outcome {
                        query,
                        result,
                        millis,
                    }));
                });
            },
            delay,
        );
    });
}

#[component]
pub fn CodePane() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    let ready = Memo::new(move |_| code.tree.with(|t| matches!(t, Load::Ready(_))));
    let status = move || {
        let pr = ctx.selected.get()?;
        let sync = ctx.sync_status(&pr.repo, pr.number);
        let view = match (code.tree.get(), sync) {
            (_, Some(SyncStatus::Queued)) => view! {
                <p class="code-status">"Waiting to sync pr-" {pr.number} "…"</p>
            }
            .into_any(),
            (_, Some(SyncStatus::Syncing { step })) => view! {
                <p class="code-status">"Preparing pr-" {pr.number} ": " {step} "…"</p>
            }
            .into_any(),
            (_, Some(SyncStatus::Failed { message })) | (Load::Failed(message), _) => view! {
                <div class="error-banner">{message}</div>
                <p class="code-status">"The Code tab reads the PR's worktree. Retrying on the next refresh."</p>
            }
            .into_any(),
            _ => view! { <p class="code-status">"Loading files…"</p> }.into_any(),
        };
        Some(view)
    };
    view! {
        <Show when=move || ready.get() fallback=status>
            <div class="code-split">
                <Explorer/>
                <Viewer/>
            </div>
            <Show when=move || code.palette.get()>
                <Palette/>
            </Show>
        </Show>
    }
}

#[component]
fn Explorer() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    let searching = Memo::new(move |_| code.search.with(|q| !q.pattern.trim().is_empty()));
    view! {
        <aside class="explorer">
            <SearchBox/>
            <div class="explorer-body">
                <Show
                    when=move || searching.get()
                    fallback=|| {
                        view! {
                            <Changed/>
                            <FileTree/>
                        }
                    }
                >
                    <Results/>
                </Show>
            </div>
        </aside>
    }
}

#[component]
fn SearchBox() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;

    let open_active = move || {
        let listed = code.listed();
        if let Some((path, line)) = listed.get(code.active.get_untracked()) {
            ctx.view_code(path.clone(), Some(*line));
            util::scroll_nearest(&format!("result-{}", code.active.get_untracked()));
        }
    };
    let on_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "ArrowDown" | "ArrowUp" => {
            ev.prevent_default();
            let count = code.listed().len();
            if count == 0 {
                return;
            }
            let step = if ev.key() == "ArrowDown" {
                1
            } else {
                count - 1
            };
            code.active.update(|a| *a = (*a + step) % count);
            open_active();
        }
        "Enter" => open_active(),
        "Escape" => {
            if code.search.with_untracked(|q| q.pattern.is_empty()) {
                util::blur("code-search");
            } else {
                code.search.update(|q| q.pattern.clear());
            }
        }
        _ => {}
    };

    let option = move |label: &'static str,
                       title: &'static str,
                       get: fn(&GrepQuery) -> bool,
                       set: fn(&mut GrepQuery, bool)| {
        view! {
            <button
                class="opt"
                class:active=move || code.search.with(get)
                title=title
                on:click=move |_| {
                    code.search
                        .update(|q| {
                            let on = !get(q);
                            q.symbol = false;
                            set(q, on);
                        })
                }
            >
                {label}
            </button>
        }
    };

    let status = move || {
        let busy = code.searching.get();
        let text = code.outcome.with(|outcome| match outcome {
            None => busy.then(|| "Searching…".to_string()),
            Some(Outcome {
                result: Err(error), ..
            }) => Some(error.clone()),
            Some(Outcome {
                result: Ok(result),
                millis,
                query,
            }) => {
                let all = result.definitions.iter().chain(&result.matches);
                let files: HashSet<&str> = all.clone().map(|m| m.path.as_str()).collect();
                let count = all.count();
                let mut text = match count {
                    0 => "No results".to_string(),
                    1 => "1 result".to_string(),
                    n if files.len() == 1 => format!("{n} results in 1 file"),
                    n => format!("{n} results in {} files", files.len()),
                };
                if result.truncated {
                    text.push_str(" (showing the first 2,000)");
                }
                if query.symbol {
                    text = format!("{text} · symbol");
                }
                Some(format!("{text} · {millis} ms"))
            }
        })?;
        let failed = code
            .outcome
            .with(|o| matches!(o, Some(Outcome { result: Err(_), .. })));
        Some(view! {
            <div class="search-status" class:busy=busy class:error=failed>{text}</div>
        })
    };

    view! {
        <div class="search-box">
            <div class="search-row">
                <input
                    id="code-search"
                    class="search"
                    type="text"
                    spellcheck="false"
                    autocomplete="off"
                    placeholder="Search the repo  ⌘⇧F"
                    prop:value=move || code.search.with(|q| q.pattern.clone())
                    on:input=move |ev| {
                        code.search
                            .update(|q| {
                                q.pattern = event_target_value(&ev);
                                q.symbol = false;
                            })
                    }
                    on:keydown=on_key
                />
                {move || {
                    code.search
                        .with(|q| !q.pattern.is_empty())
                        .then(|| {
                            view! {
                                <button
                                    class="clear"
                                    title="Clear the search (Esc)"
                                    on:click=move |_| code.search.update(|q| q.pattern.clear())
                                >
                                    "×"
                                </button>
                            }
                        })
                }}
            </div>
            <div class="search-options">
                {option("Aa", "Match case", |q| q.case_sensitive, |q, on| q.case_sensitive = on)}
                {option("W", "Whole word", |q| q.whole_word, |q, on| q.whole_word = on)}
                {option(".*", "Regular expression", |q| q.regex, |q, on| q.regex = on)}
                <input
                    class="paths"
                    type="text"
                    spellcheck="false"
                    autocomplete="off"
                    placeholder="files: *.rb, app/**, !spec/"
                    title="Comma-separated globs; ! excludes. Names without a slash match at any depth."
                    prop:value=move || code.search.with(|q| q.paths.clone())
                    on:input=move |ev| code.search.update(|q| q.paths = event_target_value(&ev))
                />
            </div>
            {status}
        </div>
    }
}

#[component]
fn Results() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    move || {
        let outcome = code.outcome.get()?;
        let result = outcome.result.ok()?;
        let mut index = 0;
        let definitions = outcome.query.symbol.then(|| {
            let empty = "No definition found; uses are below.";
            section(ctx, "Definitions", &result.definitions, &mut index, empty)
        });
        let title = if outcome.query.symbol {
            "References"
        } else {
            "Matches"
        };
        let matches = section(ctx, title, &result.matches, &mut index, "Nothing found.");
        Some(view! {
            <div class="results">
                {definitions}
                {matches}
            </div>
        })
    }
}

/// One titled list of matches, grouped by file. `index` numbers the rows
/// across sections, for ↑/↓.
fn section(
    ctx: Ctx,
    title: &'static str,
    matches: &[GrepMatch],
    index: &mut usize,
    empty: &'static str,
) -> AnyView {
    let code = ctx.code;
    let count = matches.len();
    let mut groups: Vec<(&str, Vec<&GrepMatch>)> = Vec::new();
    for m in matches {
        match groups.last_mut() {
            Some((path, list)) if *path == m.path => list.push(m),
            _ => groups.push((&m.path, vec![m])),
        }
    }
    let groups = groups
        .into_iter()
        .map(|(path, list)| {
            let (dir, name) = split_path(path);
            let first_line = list[0].line;
            let open_path = path.to_string();
            let rows = list
                .into_iter()
                .map(|m| {
                    let i = *index;
                    *index += 1;
                    let (path, line) = (m.path.clone(), m.line);
                    view! {
                        <button
                            class="result-line"
                            id=format!("result-{i}")
                            class:active=move || code.active.get() == i
                            on:click=move |_| {
                                code.active.set(i);
                                ctx.view_code(path.clone(), Some(line));
                            }
                        >
                            <span class="ln">{line}</span>
                            <span class="text">{trimmed_marks(&m.text, &m.ranges)}</span>
                        </button>
                    }
                })
                .collect_view();
            let hits = rows.len();
            view! {
                <div class="result-file">
                    <button
                        class="result-path"
                        title=path.to_string()
                        on:click=move |_| ctx.view_code(open_path.clone(), Some(first_line))
                    >
                        <span class="dir">{dir}</span>
                        <span class="name">{name}</span>
                        <span class="count">{hits}</span>
                    </button>
                    {rows}
                </div>
            }
        })
        .collect_view();
    view! {
        <div class="result-section">
            <div class="section-head">
                {title}
                <span class="count">{count}</span>
            </div>
            {(count == 0).then(|| view! { <p class="empty">{empty}</p> })}
            {groups}
        </div>
    }
    .into_any()
}

/// `app/models/user.rb` → (`app/models/`, `user.rb`).
fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// A result line without its indentation.
fn trimmed_marks(text: &str, ranges: &[(u32, u32)]) -> impl IntoView + use<> {
    let indent = (text.len() - text.trim_start().len()) as u32;
    let ranges: Vec<(u32, u32)> = ranges
        .iter()
        .filter(|&&(_, end)| end > indent)
        .map(|&(start, end)| (start.max(indent) - indent, end - indent))
        .collect();
    marked(text.trim_start(), &ranges, "hit")
}

/// `text` with `ranges` wrapped in `<mark class=…>`.
fn marked(text: &str, ranges: &[(u32, u32)], class: &'static str) -> Vec<AnyView> {
    let mut parts = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut at = 0;
    for &(start, end) in ranges {
        let (start, end) = (start as usize, end as usize);
        let (Some(before), Some(hit)) = (text.get(at..start), text.get(start..end)) else {
            continue;
        };
        if !before.is_empty() {
            parts.push(before.to_string().into_any());
        }
        parts.push(view! { <mark class=class>{hit.to_string()}</mark> }.into_any());
        at = end;
    }
    if let Some(rest) = text.get(at..).filter(|rest| !rest.is_empty()) {
        parts.push(rest.to_string().into_any());
    }
    parts
}

#[component]
fn Changed() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let selected = selected_path(ctx);
    move || {
        ctx.files.with(|files| {
            let Load::Ready(files) = files else {
                return None;
            };
            if files.is_empty() {
                return None;
            }
            let rows = files
                .iter()
                .map(|file| {
                    let (class, letter) = status_badge(file.status);
                    let (dir, name) = split_path(&file.path);
                    let path = file.path.clone();
                    let line = file.first_changed_line();
                    let removed = file.status == FileStatus::Removed;
                    let is_selected = {
                        let path = path.clone();
                        move || selected.with(|s| s.as_deref() == Some(path.as_str()))
                    };
                    view! {
                        <button
                            class="tree-row changed-row"
                            class:selected=is_selected
                            disabled=removed
                            title=if removed {
                                format!("{} was deleted in this PR", file.path)
                            } else {
                                file.path.clone()
                            }
                            on:click=move |_| ctx.view_code(path.clone(), line)
                        >
                            <span class=class>{letter}</span>
                            <span class="dir">{dir}</span>
                            <span class="name">{name}</span>
                        </button>
                    }
                })
                .collect_view();
            Some(view! {
                <div class="section-head">
                    "Changed in this PR"
                    <span class="count">{files.len()}</span>
                </div>
                <div class="changed">{rows}</div>
            })
        })
    }
}

fn selected_path(ctx: Ctx) -> Memo<Option<String>> {
    Memo::new(move |_| ctx.code.target.with(|t| t.as_ref().map(|t| t.path.clone())))
}

fn tree_id(path: &str) -> String {
    format!("tree-{path}")
}

/// What every row of the file tree needs.
#[derive(Clone, Copy)]
struct TreeEnv {
    ctx: Ctx,
    selected: Memo<Option<String>>,
    statuses: Memo<HashMap<String, FileStatus>>,
}

#[component]
fn FileTree() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    let env = TreeEnv {
        ctx,
        selected: selected_path(ctx),
        statuses: Memo::new(move |_| {
            ctx.files.with(|files| match files {
                Load::Ready(files) => files.iter().map(|f| (f.path.clone(), f.status)).collect(),
                _ => HashMap::new(),
            })
        }),
    };
    let nodes = Memo::new(move |_| {
        code.tree.with(|tree| match tree {
            Load::Ready(tree) => Some((tree.nodes.clone(), tree.paths.len())),
            _ => None,
        })
    });
    view! {
        <div class="section-head">
            "Files"
            <span class="count">{move || nodes.with(|n| n.as_ref().map(|(_, count)| *count))}</span>
            <span class="spacer"></span>
            <button
                class="icon-btn small"
                title="Collapse every folder"
                on:click=move |_| code.expanded.set(HashSet::new())
            >
                <Icon path=util::FOLD_ICON size=12/>
            </button>
            <button class="kbd" title="Go to file" on:click=move |_| code.palette.set(true)>
                "⌘P"
            </button>
        </div>
        <div class="file-tree">
            {move || nodes.get().map(|(nodes, _)| tree_rows(nodes, 0, env))}
        </div>
    }
}

fn tree_rows(nodes: Arc<[TreeNode]>, depth: usize, env: TreeEnv) -> AnyView {
    nodes
        .iter()
        .map(|node| tree_row(node.clone(), depth, env))
        .collect_view()
        .into_any()
}

fn tree_row(node: TreeNode, depth: usize, env: TreeEnv) -> AnyView {
    let TreeEnv {
        ctx,
        selected,
        statuses,
    } = env;
    let code = ctx.code;
    // Files line up with folder names, past the chevron.
    let chevron = if node.is_dir() { 0 } else { 16 };
    let indent = format!("padding-left: {}px", 8 + depth * 12 + chevron);
    let id = tree_id(&node.path);
    match node.children {
        Some(children) => {
            let path = node.path.clone();
            let open = {
                let path = path.clone();
                Memo::new(move |_| code.expanded.with(|e| e.contains(&path)))
            };
            let toggle = move |_| {
                code.expanded.update(|e| {
                    if !e.remove(&path) {
                        e.insert(path.clone());
                    }
                })
            };
            view! {
                <button class="tree-row dir" style=indent id=id title=node.path.clone() on:click=toggle>
                    <span class="chevron" class:open=move || open.get()>"▸"</span>
                    <span class="name">{node.name}</span>
                </button>
                {move || open.get().then(|| tree_rows(children.clone(), depth + 1, env))}
            }
            .into_any()
        }
        None => {
            let path = node.path.clone();
            let badge = statuses.with(|s| s.get(&path).copied()).map(|status| {
                let (class, letter) = status_badge(status);
                view! { <span class=format!("{class} tiny")>{letter}</span> }
            });
            let is_selected = {
                let path = path.clone();
                move || selected.with(|s| s.as_deref() == Some(path.as_str()))
            };
            view! {
                <button
                    class="tree-row leaf"
                    style=indent
                    id=id
                    title=node.path.clone()
                    class:selected=is_selected
                    on:click=move |_| ctx.view_code(path.clone(), None)
                >
                    <span class="name">{node.name}</span>
                    {badge}
                </button>
            }
            .into_any()
        }
    }
}

/// What a viewer line is highlighted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkKind {
    /// The diff's word-level changes.
    Emphasis,
    Search,
    Find,
    FindCurrent,
}

pub type Marks = Vec<(u32, u32, MarkKind)>;

#[component]
fn Viewer() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;

    // Find-in-file matches as (line index, start, end).
    let find_hits = Memo::new(move |_| {
        let query = code.find_query.get();
        let case = code.find_case.get();
        if !code.find_open.get() || query.is_empty() {
            return Arc::new(Vec::new());
        }
        code.file.with(|file| {
            let Load::Ready(file) = file else {
                return Arc::new(Vec::new());
            };
            Arc::new(find_all(
                file.lines.iter().map(String::as_str),
                &query,
                case,
            ))
        })
    });

    // Bring the target line into view once its file is showing.
    Effect::new(move |_| {
        let target = code.target.get();
        let showing = code.file.with(|f| match f {
            Load::Ready(file) => Some(file.content.path.clone()),
            _ => None,
        });
        if let (Some(target), Some(showing)) = (target, showing)
            && target.path == showing
        {
            set_timeout(
                move || match target.line {
                    Some(line) => go_to_line(line, true),
                    None => util::scroll_top("viewer-body"),
                },
                Duration::ZERO,
            );
        }
    });
    // A new file starts at its first find match.
    Effect::new(move |previous: Option<Option<String>>| {
        let path = code.file.with(|f| match f {
            Load::Ready(file) => Some(file.content.path.clone()),
            _ => None,
        });
        if previous.is_some_and(|previous| previous != path) {
            code.find_index.set(0);
        }
        path
    });
    // And step through find matches.
    Effect::new(move |_| {
        let index = code.find_index.get();
        if let Some(&(line, _, _)) = find_hits.with(|hits| hits.get(index).copied()).as_ref() {
            go_to_line(line + 1, false);
        }
    });

    let body = move || match code.file.get() {
        Load::Idle => view! { <ViewerHints/> }.into_any(),
        Load::Loading => view! { <p class="loading">"Loading…"</p> }.into_any(),
        Load::Failed(error) => view! { <div class="error-banner">{error}</div> }.into_any(),
        Load::Ready(file) => match &file.content.body {
            FileBody::Text(_) => {
                let path = file.content.path.clone();
                let changes = ctx.files.with(|files| match files {
                    Load::Ready(files) => files
                        .iter()
                        .find(|f| f.path == path)
                        .map(|f| f.head_changes())
                        .unwrap_or_default(),
                    _ => HeadChanges::default(),
                });
                let threads: HashMap<u32, (usize, usize, bool)> = ctx.detail.with(|detail| {
                    let Load::Ready(detail) = detail else {
                        return HashMap::new();
                    };
                    markers(detail)
                        .remove(&path)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|((side, _), _)| *side == Side::New)
                        .map(|((_, line), found)| {
                            let comments = found.iter().map(|m| m.comments).sum();
                            let resolved = found.iter().all(|m| m.resolved);
                            (line, (found[0].thread, comments, resolved))
                        })
                        .collect()
                });
                view! { <CodeLines file changes threads find_hits/> }.into_any()
            }
            FileBody::Binary => view! {
                <p class="file-note">"Binary file, " {util::bytes(file.content.size)} "."</p>
            }
            .into_any(),
            FileBody::TooLarge => {
                let path = file.content.path.clone();
                view! {
                    <p class="file-note">
                        "Too large to show here (" {util::bytes(file.content.size)} "). "
                        <button
                            class="link"
                            on:click=move |_| ctx.open_in_zed(Some(path.clone()), None)
                        >
                            "Open it in Zed"
                        </button>
                    </p>
                }
                .into_any()
            }
        },
    };

    view! {
        <section class="viewer">
            <ViewerHeader/>
            <Show when=move || code.find_open.get()>
                <FindBar hits=find_hits/>
            </Show>
            <div class="viewer-body" id="viewer-body">{body}</div>
        </section>
    }
}

/// Scrolls the viewer to a 1-based line; `flash` marks it as the target.
fn go_to_line(line: u32, flash: bool) {
    let id = format!("code-L{line}");
    if flash {
        util::reveal_now(&id);
    } else {
        util::scroll_center(&id);
    }
    if flash {
        mark_current(line);
    }
}

/// The current line keeps a quiet highlight after the flash.
fn mark_current(line: u32) {
    for row in util::query_all(".code-table tr.current") {
        let _ = row.class_list().remove_1("current");
    }
    if let Some(row) = document().get_element_by_id(&format!("code-L{line}")) {
        let _ = row.class_list().add_1("current");
    }
}

/// Case-insensitive matching folds ASCII only, which keeps byte offsets
/// valid for the original line.
pub fn find_all<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    query: &str,
    case_sensitive: bool,
) -> Vec<(u32, u32, u32)> {
    let query = if case_sensitive {
        query.to_string()
    } else {
        query.to_ascii_lowercase()
    };
    let mut hits = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        let haystack = if case_sensitive {
            std::borrow::Cow::Borrowed(line)
        } else {
            std::borrow::Cow::Owned(line.to_ascii_lowercase())
        };
        let mut from = 0;
        while let Some(at) = haystack[from..].find(&query) {
            let start = from + at;
            let end = start + query.len();
            hits.push((index as u32, start as u32, end as u32));
            from = end;
        }
    }
    hits
}

#[component]
fn ViewerHints() -> impl IntoView {
    view! {
        <div class="viewer-hints">
            <p>"Pick a file on the left, or:"</p>
            <dl>
                <dt>"⌘P"</dt>
                <dd>"go to file"</dd>
                <dt>"⌘⇧F"</dt>
                <dd>"search the repo, or the selected text"</dd>
                <dt>"⌘-click"</dt>
                <dd>"a name, in the diff or here, to find its definition"</dd>
                <dt>"⌘F"</dt>
                <dd>"find in the open file"</dd>
                <dt>"⌘1 ⌘2"</dt>
                <dd>"switch between Diff and Code"</dd>
            </dl>
        </div>
    }
}

#[component]
fn ViewerHeader() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    move || {
        let (path, sha) = code.file.with(|f| match f {
            Load::Ready(file) => Some((file.content.path.clone(), file.content.sha.clone())),
            _ => None,
        })?;
        let (dir, name) = split_path(&path);
        let in_diff = ctx.files.with(|files| match files {
            Load::Ready(files) => files.iter().any(|f| f.path == path),
            _ => false,
        });
        let show_in_diff = {
            let path = path.clone();
            move |_| {
                let anchor = code.cursor.get_untracked().map(|line| (Side::New, line));
                ctx.jump_to(&path, anchor);
            }
        };
        let zed = {
            let path = path.clone();
            move |_| ctx.open_in_zed(Some(path.clone()), code.cursor.get_untracked())
        };
        let zed_title = move || match code.cursor.get() {
            Some(line) => format!("Open this file in Zed, at line {line}"),
            None => "Open this file in Zed".to_string(),
        };
        Some(view! {
            <header class="viewer-head">
                <span class="crumbs" title=path.clone()>
                    <span class="dir">{dir}</span>
                    <span class="name">{name}</span>
                </span>
                <code class="sha" title=sha.clone()>"@ " {sha[..7].to_string()}</code>
                <span class="spacer"></span>
                <button class="btn small icon" title="Find in this file (⌘F)" on:click=move |_| ctx.find_in_file()>
                    <Icon path=util::SEARCH_ICON size=14/>
                </button>
                {in_diff
                    .then(|| {
                        view! {
                            <button class="btn small icon" title="Show this file in the diff" on:click=show_in_diff>
                                <Icon path=util::DIFF_ICON size=14/>
                            </button>
                        }
                    })}
                <button
                    class="btn small icon"
                    disabled=move || ctx.zed_busy.get()
                    title=zed_title
                    on:click=zed
                >
                    <Icon path=util::ZED_ICON size=14/>
                </button>
            </header>
        })
    }
}

#[component]
fn FindBar(hits: Memo<Arc<Vec<(u32, u32, u32)>>>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    let step = move |forward: bool| {
        let count = hits.with_untracked(|h| h.len());
        if count == 0 {
            return;
        }
        code.find_index.update(|i| {
            *i = if forward {
                (*i + 1) % count
            } else {
                (*i + count - 1) % count
            }
        });
    };
    let close = move || {
        code.find_open.set(false);
        util::blur("find-input");
    };
    let on_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            ev.prevent_default();
            step(!ev.shift_key());
        }
        "Escape" => close(),
        _ => {}
    };
    let count = move || {
        let total = hits.with(|h| h.len());
        if code.find_query.with(String::is_empty) {
            String::new()
        } else if total == 0 {
            "No results".into()
        } else {
            format!("{}/{total}", code.find_index.get().min(total - 1) + 1)
        }
    };
    view! {
        <div class="find-bar">
            <input
                id="find-input"
                class="search"
                type="text"
                spellcheck="false"
                autocomplete="off"
                placeholder="Find in file"
                prop:value=move || code.find_query.get()
                on:input=move |ev| {
                    code.find_query.set(event_target_value(&ev));
                    code.find_index.set(0);
                }
                on:keydown=on_key
            />
            <span class="find-count">{count}</span>
            <button class="opt" title="Previous (⇧↵)" on:click=move |_| step(false)>"↑"</button>
            <button class="opt" title="Next (↵)" on:click=move |_| step(true)>"↓"</button>
            <button
                class="opt"
                class:active=move || code.find_case.get()
                title="Match case"
                on:click=move |_| {
                    code.find_case.update(|c| *c = !*c);
                    code.find_index.set(0);
                }
            >
                "Aa"
            </button>
            <button class="opt" title="Close (Esc)" on:click=move |_| close()>"×"</button>
        </div>
    }
}

#[component]
fn CodeLines(
    file: Arc<OpenFile>,
    changes: HeadChanges,
    threads: HashMap<u32, (usize, usize, bool)>,
    find_hits: Memo<Arc<Vec<(u32, u32, u32)>>>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    let path = file.content.path.clone();

    // Each line's highlights live in their own signal, so a new search or
    // find query only re-renders the lines whose highlights changed.
    let marks: Arc<Vec<RwSignal<Marks>>> = Arc::new(
        file.lines
            .iter()
            .map(|_| RwSignal::new(Marks::new()))
            .collect(),
    );
    let shown = StoredValue::new(HashMap::<usize, Marks>::new());
    {
        let (marks, file) = (marks.clone(), file.clone());
        Effect::new(move |_| {
            let mut wanted: HashMap<usize, Marks> = HashMap::new();
            let hits = find_hits.get();
            if !hits.is_empty() {
                let current = code.find_index.get();
                for (i, &(line, start, end)) in hits.iter().enumerate() {
                    let kind = if i == current {
                        MarkKind::FindCurrent
                    } else {
                        MarkKind::Find
                    };
                    wanted
                        .entry(line as usize)
                        .or_default()
                        .push((start, end, kind));
                }
            } else {
                code.outcome.with(|outcome| {
                    let Some(Outcome {
                        result: Ok(result), ..
                    }) = outcome
                    else {
                        return;
                    };
                    for m in result.definitions.iter().chain(&result.matches) {
                        if m.path != path {
                            continue;
                        }
                        let index = m.line.saturating_sub(1) as usize;
                        if let Some(line) = file.lines.get(index) {
                            let ranges = align(line, m);
                            if !ranges.is_empty() {
                                let entry = wanted.entry(index).or_default();
                                entry.extend(
                                    ranges.into_iter().map(|(s, e)| (s, e, MarkKind::Search)),
                                );
                                entry.sort_unstable();
                                entry.dedup();
                            }
                        }
                    }
                });
            }
            shown.update_value(|shown| {
                for (index, list) in &wanted {
                    if shown.get(index) != Some(list) {
                        marks[*index].set(list.clone());
                    }
                }
                for index in shown.keys() {
                    if !wanted.contains_key(index) {
                        marks[*index].set(Marks::new());
                    }
                }
                *shown = wanted;
            });
        });
    }

    let rows = file
        .lines
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let number = index as u32 + 1;
            let line_marks = marks[index];
            let text = text.clone();
            let tokens = file.tokens.get(index).cloned().unwrap_or_default();
            let bubble = threads.get(&number).map(|&(thread, comments, resolved)| {
                view! {
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
                }
            });
            let select = move |_| {
                code.cursor.set(Some(number));
                mark_current(number);
            };
            let class = match (
                changes.added.contains(&number),
                changes.deleted_before.contains(&number),
            ) {
                (true, true) => "added deleted-above",
                (true, false) => "added",
                (false, true) => "deleted-above",
                (false, false) => "",
            };
            view! {
                <tr id=format!("code-L{number}") class=class>
                    <td class="num" on:click=select>
                        {bubble}
                        {number}
                    </td>
                    <td class="code">
                        {move || line_marks.with(|m| paint(&text, &tokens, m))}
                    </td>
                </tr>
            }
        })
        .collect_view();

    // A deletion after the last line has no line to sit on.
    let deleted_at_end = changes
        .deleted_before
        .contains(&(file.lines.len() as u32 + 1));
    view! {
        <table class="code-table">
            <colgroup>
                <col class="c-num"/>
                <col/>
            </colgroup>
            <tbody>{rows}</tbody>
        </table>
        {deleted_at_end.then(|| view! { <div class="deleted-at-end" title="Lines removed at the end"></div> })}
    }
}

/// A line with its syntax highlighting and, over that, search and find
/// marks. Where the two overlap, the line is cut at both kinds of boundary.
pub fn paint(text: &str, tokens: &Tokens, marks: &Marks) -> Vec<AnyView> {
    let token = |piece: &str, class: Option<&'static str>| match class {
        Some(class) => view! { <span class=class>{piece.to_string()}</span> }.into_any(),
        None => piece.to_string().into_any(),
    };
    if marks.is_empty() {
        let mut parts = Vec::with_capacity(tokens.len() * 2 + 1);
        let mut at = 0;
        for &(start, end, class) in tokens {
            let (start, end) = (start as usize, end as usize);
            let (Some(before), Some(piece)) = (text.get(at..start), text.get(start..end)) else {
                continue;
            };
            if !before.is_empty() {
                parts.push(token(before, None));
            }
            parts.push(token(piece, Some(class)));
            at = end;
        }
        if let Some(rest) = text.get(at..).filter(|rest| !rest.is_empty()) {
            parts.push(token(rest, None));
        }
        return parts;
    }

    let mut cuts: Vec<u32> = tokens
        .iter()
        .flat_map(|&(s, e, _)| [s, e])
        .chain(marks.iter().flat_map(|&(s, e, _)| [s, e]))
        .chain([0, text.len() as u32])
        .filter(|&cut| cut as usize <= text.len())
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    // Consecutive pieces under the same mark share one <mark>, so a hit
    // reads as one box however many tokens it spans.
    let mut groups: Vec<(Option<MarkKind>, Vec<AnyView>)> = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let Some(piece) = text.get(start as usize..end as usize) else {
            continue;
        };
        let class = tokens
            .iter()
            .find(|t| t.0 <= start && start < t.1)
            .map(|t| t.2);
        let mark = marks
            .iter()
            .find(|m| m.0 <= start && start < m.1)
            .map(|m| m.2);
        match groups.last_mut() {
            Some((last, pieces)) if *last == mark => pieces.push(token(piece, class)),
            _ => groups.push((mark, vec![token(piece, class)])),
        }
    }
    groups
        .into_iter()
        .flat_map(|(mark, pieces)| match mark {
            None => pieces,
            Some(kind) => {
                let class = match kind {
                    MarkKind::Emphasis => "emphasis",
                    MarkKind::Search => "hit",
                    MarkKind::Find => "hit find",
                    MarkKind::FindCurrent => "hit find current",
                };
                vec![view! { <mark class=class>{pieces}</mark> }.into_any()]
            }
        })
        .collect()
}

/// A match's ranges on the real line. Long lines arrive shortened around
/// the match, so their ranges are found again by the text they cover.
fn align(line: &str, m: &GrepMatch) -> Vec<(u32, u32)> {
    if m.text == line {
        return m.ranges.clone();
    }
    let mut ranges = Vec::new();
    let mut from = 0;
    for &(start, end) in &m.ranges {
        let Some(needle) = m.text.get(start as usize..end as usize) else {
            continue;
        };
        if let Some(at) = line.get(from..).and_then(|rest| rest.find(needle)) {
            let start = from + at;
            ranges.push((start as u32, (start + needle.len()) as u32));
            from = start + needle.len();
        }
    }
    ranges
}

#[component]
fn Palette() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let code = ctx.code;
    let query = RwSignal::new(String::new());
    let selected = RwSignal::new(0usize);
    let changed = Memo::new(move |_| {
        ctx.files.with(|files| match files {
            Load::Ready(files) => files
                .iter()
                .filter(|f| f.status != FileStatus::Removed)
                .map(|f| f.path.clone())
                .collect::<HashSet<_>>(),
            _ => HashSet::new(),
        })
    });
    let results = Memo::new(move |_| {
        let query = query.get();
        let changed = changed.get();
        code.tree.with(|tree| {
            let Load::Ready(tree) = tree else {
                return Vec::new();
            };
            if query.trim().is_empty() {
                let mut first: Vec<&String> =
                    tree.paths.iter().filter(|p| changed.contains(*p)).collect();
                first.extend(tree.paths.iter().filter(|p| !changed.contains(*p)));
                return first
                    .into_iter()
                    .take(PALETTE_ROWS)
                    .map(|p| (p.clone(), Vec::new(), changed.contains(p)))
                    .collect();
            }
            let mut scored: Vec<(i32, &String, Vec<usize>)> = tree
                .paths
                .iter()
                .filter_map(|path| {
                    let (score, at) = fuzzy_score(&query, path)?;
                    // Files in the PR are the likeliest to be wanted.
                    let bonus = if changed.contains(path) { 10 } else { 0 };
                    Some((score + bonus, path, at))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
            scored
                .into_iter()
                .take(PALETTE_ROWS)
                .map(|(_, path, at)| (path.clone(), at, changed.contains(path)))
                .collect::<Vec<_>>()
        })
    });
    let close = move || code.palette.set(false);
    let open = move |index: usize| {
        if let Some((path, _, _)) = results.with_untracked(|r| r.get(index).cloned()) {
            close();
            ctx.view_code(path, None);
        }
    };
    let on_key = move |ev: KeyboardEvent| {
        let count = results.with_untracked(Vec::len);
        match ev.key().as_str() {
            "ArrowDown" | "ArrowUp" if count > 0 => {
                ev.prevent_default();
                let step = if ev.key() == "ArrowDown" {
                    1
                } else {
                    count - 1
                };
                selected.update(|s| *s = (*s + step) % count);
                util::scroll_nearest(&format!("palette-{}", selected.get_untracked()));
            }
            "Enter" => open(selected.get_untracked()),
            "Escape" => close(),
            _ => {}
        }
    };
    request_animation_frame(|| util::focus("palette-input"));

    view! {
        <div class="palette-backdrop" on:click=move |_| close()>
            <div class="palette" on:click=|ev| ev.stop_propagation()>
                <input
                    id="palette-input"
                    class="search"
                    type="text"
                    spellcheck="false"
                    autocomplete="off"
                    placeholder="Go to file"
                    prop:value=move || query.get()
                    on:input=move |ev| {
                        query.set(event_target_value(&ev));
                        selected.set(0);
                    }
                    on:keydown=on_key
                />
                <div class="palette-list">
                    {move || {
                        let rows = results.get();
                        if rows.is_empty() {
                            return view! { <p class="empty">"No matching files."</p> }.into_any();
                        }
                        rows.into_iter()
                            .enumerate()
                            .map(|(index, (path, at, is_changed))| {
                                let (dir, name) = split_path(&path);
                                let name_start = dir.len();
                                let dir_marks = fuzzy_marks(&dir, &at, 0);
                                let name_marks = fuzzy_marks(&name, &at, name_start);
                                view! {
                                    <button
                                        class="palette-row"
                                        id=format!("palette-{index}")
                                        class:active=move || selected.get() == index
                                        on:mousemove=move |_| {
                                            if selected.get_untracked() != index {
                                                selected.set(index);
                                            }
                                        }
                                        on:click=move |_| open(index)
                                    >
                                        <span class="name">{marked(&name, &name_marks, "hit")}</span>
                                        <span class="dir">{marked(&dir, &dir_marks, "hit")}</span>
                                        {is_changed.then(|| view! { <span class="chip">"changed"</span> })}
                                    </button>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </div>
                <footer class="palette-foot">"↑↓ move · ↵ open · esc close"</footer>
            </div>
        </div>
    }
}

/// Matched character offsets of the full path that fall in `part`, which
/// starts at byte `offset` of it, as ranges of `part`.
fn fuzzy_marks(part: &str, at: &[usize], offset: usize) -> Vec<(u32, u32)> {
    at.iter()
        .filter(|&&i| i >= offset && i < offset + part.len())
        .filter_map(|&i| {
            let local = i - offset;
            let len = part[local..].chars().next()?.len_utf8();
            Some((local as u32, (local + len) as u32))
        })
        .collect()
}
