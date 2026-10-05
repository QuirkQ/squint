use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use leptos::ev::MouseEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::{Deserialize, Serialize};
use shared::{
    DiffShas, FileDiff, Inbox, PullDetail, PullSummary, RoundEvent, Side, SyncEvent, SyncStatus,
};
use wasm_bindgen::JsCast;

use crate::actions::{self, ActionsState};
use crate::code_view::{self, CodeState};
use crate::comments::{self, CommentsState};
use crate::conversation::Conversation;
use crate::diff_view::{DiffPane, default_collapsed};
use crate::layout::{self, Layout};
use crate::review_bar::ReviewBar;
use crate::settings::Settings;
use crate::sidebar::Sidebar;
use crate::{api, util};

const INBOX_KEY: &str = "inbox";
const MODE_KEY: &str = "diff-mode";
const SIDE_TAB_KEY: &str = "side-tab";
const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffMode {
    Unified,
    Split,
}

/// The middle pane's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Diff,
    Code,
    /// A GitHub Actions job's log, opened from the Checks pane.
    Logs,
}

/// The right pane's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideTab {
    Conversation,
    Checks,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Load<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub text: String,
    pub error: bool,
}

/// State shared by the three panes, provided as context.
#[derive(Clone, Copy)]
pub struct Ctx {
    pub selected: RwSignal<Option<PullSummary>>,
    pub detail: RwSignal<Load<Arc<PullDetail>>>,
    pub files: RwSignal<Load<Arc<Vec<FileDiff>>>>,
    pub mode: RwSignal<DiffMode>,
    /// Paths of files whose diff is folded away.
    pub collapsed: RwSignal<HashSet<String>>,
    pub settings_open: RwSignal<bool>,
    pub zed_busy: RwSignal<bool>,
    /// Each pull request's worktree status in the review workspace.
    pub sync: RwSignal<HashMap<(String, u32), SyncStatus>>,
    /// A sync round is running.
    pub syncing: RwSignal<bool>,
    pub tab: RwSignal<Tab>,
    pub side_tab: RwSignal<SideTab>,
    /// What the diff compares, once the PR's worktree is synced; it's what
    /// lets the diff be syntax highlighted.
    pub diff_shas: RwSignal<Load<DiffShas>>,
    pub code: CodeState,
    pub actions: ActionsState,
    pub layout: Layout,
    pub comments: CommentsState,
    toast: RwSignal<Option<(u32, Toast)>>,
}

impl Ctx {
    pub fn sync_status(&self, repo: &str, number: u32) -> Option<SyncStatus> {
        self.sync
            .with(|s| s.get(&(repo.to_string(), number)).cloned())
    }

    /// What the Zed button says while busy: the worktree's current step.
    pub fn zed_progress(&self) -> Option<String> {
        if !self.zed_busy.get() {
            return None;
        }
        let step = self.selected.with(|pr| {
            pr.as_ref()
                .and_then(|pr| self.sync_status(&pr.repo, pr.number))
        });
        Some(match step {
            Some(SyncStatus::Syncing { step }) => {
                let mut label: String = step[..1].to_uppercase() + &step[1..];
                label.push('…');
                label
            }
            _ => "Opening…".into(),
        })
    }

    pub fn toast(&self) -> Option<Toast> {
        self.toast
            .with(|t| t.as_ref().map(|(_, toast)| toast.clone()))
    }

    pub fn notify(&self, text: impl Into<String>, error: bool) {
        let id = self
            .toast
            .with_untracked(|t| t.as_ref().map_or(0, |(id, _)| id + 1));
        let toast = Toast {
            text: text.into(),
            error,
        };
        self.toast.set(Some((id, toast)));
        let slot = self.toast;
        set_timeout(
            move || {
                if slot.with_untracked(|t| t.as_ref().is_some_and(|(current, _)| *current == id)) {
                    slot.set(None);
                }
            },
            Duration::from_secs(if error { 8 } else { 4 }),
        );
    }

    /// Where a comment thread's location link goes: the line in the Code tab
    /// when that's open (and the thread is on the head side), else the diff.
    pub fn show_location(&self, path: &str, anchor: Option<(Side, u32)>) {
        match anchor {
            Some((Side::New, line)) if self.tab.get_untracked() == Tab::Code => {
                self.view_code(path.to_string(), Some(line))
            }
            _ => self.jump_to(path, anchor),
        }
    }

    /// Fetches the conversation again, e.g. to show a review just sent, if
    /// `repo#number` is still the pull request shown.
    pub fn reload_detail(&self, repo: String, number: u32) {
        let ctx = *self;
        spawn_local(async move {
            let Ok(detail) = api::pull_detail(&repo, number).await else {
                return;
            };
            let shown = ctx.selected.with_untracked(|pr| {
                pr.as_ref()
                    .is_some_and(|pr| pr.repo == repo && pr.number == number)
            });
            if shown {
                ctx.detail.set(Load::Ready(Arc::new(detail)));
            }
        });
    }

    /// Scrolls the conversation to a review thread, showing the pane first.
    pub fn show_thread(&self, index: usize) {
        let id = format!("thread-{index}");
        let showing = self.layout.conversation.open.get_untracked()
            && self.side_tab.get_untracked() == SideTab::Conversation;
        if showing {
            util::reveal(&id);
        } else {
            self.layout.conversation.open.set(true);
            self.side_tab.set(SideTab::Conversation);
            set_timeout(
                move || {
                    util::reveal(&id);
                },
                Duration::from_millis(30),
            );
        }
    }

    /// Scrolls the diff to a file, or to a line in it, unfolding it first.
    pub fn jump_to(&self, path: &str, anchor: Option<(Side, u32)>) {
        self.tab.set(Tab::Diff);
        let index = self.files.with_untracked(|files| match files {
            Load::Ready(files) => files.iter().position(|f| f.path == path),
            _ => None,
        });
        let Some(index) = index else {
            self.notify(format!("{path} is not part of the current diff"), true);
            return;
        };
        self.collapsed.update(|c| {
            c.remove(path);
        });
        let ctx = *self;
        // Give the unfolded file a moment to render before scrolling.
        set_timeout(
            move || {
                let found =
                    anchor.is_some_and(|(side, line)| util::reveal(&line_id(index, side, line)));
                if !found {
                    util::reveal(&format!("file-{index}"));
                }
                if let (false, Some((_, line))) = (found, anchor) {
                    // GitHub allows comments on context expanded beyond the patch.
                    ctx.notify(format!("Line {line} is outside the diff's hunks"), false);
                }
            },
            Duration::from_millis(30),
        );
    }

    pub fn open_in_zed(&self, path: Option<String>, line: Option<u32>) {
        let Some(pr) = self.selected.get_untracked() else {
            return;
        };
        let ctx = *self;
        ctx.zed_busy.set(true);
        spawn_local(async move {
            match api::open_in_zed(&pr.repo, pr.number, path.as_deref(), line).await {
                Ok(launch) => match launch.warning {
                    Some(warning) => ctx.notify(warning, true),
                    None => ctx.notify(format!("Opened {}#{} in Zed", pr.repo, pr.number), false),
                },
                Err(error) => ctx.notify(error, true),
            }
            ctx.zed_busy.set(false);
        });
    }
}

pub fn line_id(file: usize, side: Side, line: u32) -> String {
    match side {
        Side::Old => format!("f{file}-o{line}"),
        Side::New => format!("f{file}-n{line}"),
    }
}

/// Refetches into `target` whenever `key` changes, dropping responses that
/// arrive after the user has already moved on to another pull request.
fn load_on_change<K, T, F, Fut>(key: Memo<Option<K>>, target: RwSignal<Load<T>>, fetch: F)
where
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
        target.set(Load::Loading);
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

#[component]
pub fn App() -> impl IntoView {
    let ctx = Ctx {
        selected: RwSignal::new(None),
        detail: RwSignal::new(Load::Idle),
        files: RwSignal::new(Load::Idle),
        mode: RwSignal::new(util::load(MODE_KEY).unwrap_or(DiffMode::Unified)),
        collapsed: RwSignal::new(HashSet::new()),
        settings_open: RwSignal::new(false),
        zed_busy: RwSignal::new(false),
        sync: RwSignal::new(HashMap::new()),
        syncing: RwSignal::new(false),
        tab: RwSignal::new(Tab::Diff),
        diff_shas: RwSignal::new(Load::Idle),
        side_tab: RwSignal::new(util::load(SIDE_TAB_KEY).unwrap_or(SideTab::Conversation)),
        code: CodeState::default(),
        actions: ActionsState::default(),
        layout: Layout::default(),
        comments: CommentsState::default(),
        toast: RwSignal::new(None),
    };
    provide_context(ctx);

    // The review workspace reports progress as events; the snapshot covers
    // whatever happened before this page loaded (e.g. after a hot reload).
    api::on_event("workspace-sync", move |event: SyncEvent| {
        ctx.sync.update(|s| {
            s.insert((event.repo, event.number), event.status);
        })
    });
    api::on_event("workspace-round", move |event: RoundEvent| {
        ctx.syncing.set(event.running)
    });
    spawn_local(async move {
        if let Ok(summary) = api::workspace_status().await {
            ctx.syncing.set(summary.round_running);
            ctx.sync.update(|s| {
                for event in summary.statuses {
                    s.entry((event.repo, event.number)).or_insert(event.status);
                }
            });
        }
    });

    let key = Memo::new(move |_| {
        ctx.selected
            .with(|s| s.as_ref().map(|pr| (pr.repo.clone(), pr.number)))
    });
    load_on_change(key, ctx.detail, |(repo, number)| async move {
        api::pull_detail(&repo, number).await.map(Arc::new)
    });
    load_on_change(key, ctx.files, |(repo, number)| async move {
        api::pull_files(&repo, number).await.map(Arc::new)
    });
    // The selected pull request jumps the workspace's sync queue. Failures
    // show up as its sync status, so the result itself can be ignored.
    Effect::new(move |_| {
        if let Some((repo, number)) = key.get() {
            spawn_local(async move {
                let _ = api::ensure_pr(&repo, number).await;
            });
        }
    });
    Effect::new(move |_| {
        ctx.files.with(|files| {
            if let Load::Ready(files) = files {
                ctx.collapsed.set(default_collapsed(files));
            }
        })
    });
    Effect::new(move |_| util::store(MODE_KEY, &ctx.mode.get()));
    Effect::new(move |_| util::store(SIDE_TAB_KEY, &ctx.side_tab.get()));
    code_view::setup(ctx, key);
    actions::setup(ctx, key);
    comments::setup(ctx);
    layout::setup(ctx.layout);

    // The last inbox shows instantly; GitHub's search takes a few seconds.
    let inbox = RwSignal::new(util::load::<Inbox>(INBOX_KEY));
    let inbox_error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let refresh = Callback::new(move |()| {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        spawn_local(async move {
            match api::inbox().await {
                Ok(fresh) => {
                    util::store(INBOX_KEY, &fresh);
                    inbox.set(Some(fresh));
                    inbox_error.set(None);
                }
                Err(error) => inbox_error.set(Some(error)),
            }
            loading.set(false);
        });
    });

    spawn_local(async move {
        if let Ok(None) = api::token_source().await {
            inbox.set(None);
            ctx.settings_open.set(true);
        } else {
            refresh.run(());
        }
    });
    let interval = set_interval_with_handle(move || refresh.run(()), REFRESH_EVERY).ok();
    on_cleanup(move || {
        if let Some(interval) = interval {
            interval.clear();
        }
    });

    // Zed's shortcuts. j / k live in the sidebar.
    let command_held = RwSignal::new(false);
    let shortcuts = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Meta" || ev.key() == "Control" {
            command_held.set(true);
        }
        if !(ev.meta_key() || ev.ctrl_key()) || ev.alt_key() || ctx.settings_open.get_untracked() {
            return;
        }
        let has_pr = ctx.selected.with_untracked(Option::is_some);
        let handled = match (ev.key().to_lowercase().as_str(), ev.shift_key()) {
            ("1", false) => {
                ctx.tab.set(Tab::Diff);
                true
            }
            ("2", false) => {
                ctx.tab.set(Tab::Code);
                true
            }
            ("3", false) => {
                ctx.tab.set(Tab::Logs);
                true
            }
            ("p", false) if has_pr => {
                ctx.tab.set(Tab::Code);
                ctx.code.palette.set(true);
                true
            }
            ("f", true) if has_pr => {
                ctx.search_code(util::selected_text());
                true
            }
            ("f", false) if ctx.tab.get_untracked() == Tab::Code => {
                ctx.find_in_file();
                true
            }
            ("f", false) if ctx.tab.get_untracked() == Tab::Logs => {
                util::focus("log-search");
                true
            }
            _ => false,
        };
        if handled {
            ev.prevent_default();
        }
    });
    let releases = window_event_listener(leptos::ev::keyup, move |ev| {
        if ev.key() == "Meta" || ev.key() == "Control" {
            command_held.set(false);
        }
    });
    let blurs = window_event_listener(leptos::ev::blur, move |_| command_held.set(false));
    on_cleanup(move || {
        shortcuts.remove();
        releases.remove();
        blurs.remove();
    });

    let on_click = move |ev: MouseEvent| {
        let target = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok());

        // ⌘-click a name in the diff or the viewer: go to its definition.
        let in_code = target
            .as_ref()
            .and_then(|el| el.closest(".diff-pane td.code").ok().flatten())
            .is_some();
        if in_code
            && (ev.meta_key() || ev.ctrl_key())
            && let Some(word) = util::word_at_point(ev.client_x(), ev.client_y())
        {
            ev.prevent_default();
            if shared::code::is_symbol(&word) {
                ctx.find_symbol(word);
            } else {
                ctx.search_code(Some(word));
            }
            return;
        }

        // Comment bodies are GitHub HTML full of links; never navigate the app
        // window itself, open them in the browser instead.
        let anchor = target.and_then(|el| el.closest("a[href]").ok().flatten());
        let Some(anchor) = anchor else {
            return;
        };
        let href = anchor.get_attribute("href").unwrap_or_default();
        // A comment's link to this repository's Actions ("read the full
        // plan in Actions") opens in the Logs tab.
        let in_comment = anchor.closest(".markdown-body").ok().flatten().is_some();
        let pr_repo = ctx
            .selected
            .with_untracked(|pr| pr.as_ref().map(|pr| pr.repo.to_lowercase()));
        if in_comment
            && let Some((repo, run, job)) = shared::actions::parse_actions_url(&href)
            && pr_repo == Some(repo.to_lowercase())
        {
            ev.prevent_default();
            ctx.open_job(run, job);
            return;
        }
        if !href.starts_with('#') {
            ev.prevent_default();
            util::open_external(href);
        }
    };

    view! {
        <div
            class="app"
            class:command=move || command_held.get()
            class:resizing=move || ctx.layout.resizing()
            style:grid-template-columns=move || ctx.layout.columns()
            on:click=on_click
        >
            <Sidebar inbox=inbox error=inbox_error loading=loading on_refresh=refresh/>
            <DiffPane/>
            <Conversation/>
            <ReviewBar on_done=refresh/>
            <Show when=move || ctx.settings_open.get()>
                <Settings on_saved=refresh/>
            </Show>
            {move || {
                ctx.toast()
                    .map(|toast| {
                        view! {
                            <div class="toast" class:error=toast.error role="status">
                                {toast.text}
                            </div>
                        }
                    })
            }}
        </div>
    }
}
