use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};

use leptos::prelude::*;
use shared::{CheckState, Inbox, PullSummary, ReviewDecision, Role, SyncStatus};
use wasm_bindgen::JsCast;

use crate::app::Ctx;
use crate::layout::ResizeHandle;
use crate::util::{self, Avatar, PullIcon, RepoIcon};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    ReviewRequested,
    Mine,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::All, Filter::ReviewRequested, Filter::Mine];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::ReviewRequested => "To review",
            Filter::Mine => "Mine",
        }
    }

    fn matches(self, pr: &PullSummary) -> bool {
        match self {
            Filter::All => true,
            Filter::ReviewRequested => pr.has_role(Role::ReviewRequested),
            Filter::Mine => pr.has_role(Role::Author) || pr.has_role(Role::Assignee),
        }
    }
}

fn matches_query(pr: &PullSummary, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let number = query.trim_start_matches('#');
    pr.number.to_string() == number
        || pr.title.to_lowercase().contains(&query)
        || pr.repo.to_lowercase().contains(&query)
        || pr.head_ref.to_lowercase().contains(&query)
        || pr
            .author
            .as_ref()
            .is_some_and(|a| a.login.to_lowercase().contains(&query))
}

#[component]
pub fn Sidebar(
    inbox: RwSignal<Option<Inbox>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    on_refresh: Callback<()>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let query = RwSignal::new(String::new());
    let filter = RwSignal::new(Filter::All);
    let show_drafts = RwSignal::new(true);
    let folded_repos = RwSignal::new(HashSet::<String>::new());

    let visible = move |pr: &PullSummary| {
        (show_drafts.get() || !pr.is_draft) && matches_query(pr, &query.get())
    };
    let count = move |f: Filter| {
        inbox.with(|inbox| {
            inbox.as_ref().map_or(0, |inbox| {
                inbox
                    .pulls
                    .iter()
                    .filter(|pr| f.matches(pr) && visible(pr))
                    .count()
            })
        })
    };
    let groups = Memo::new(move |_| {
        inbox.with(|inbox| {
            let mut groups: BTreeMap<String, Vec<PullSummary>> = BTreeMap::new();
            for pr in inbox.iter().flat_map(|i| &i.pulls) {
                if filter.get().matches(pr) && visible(pr) {
                    groups.entry(pr.repo.clone()).or_default().push(pr.clone());
                }
            }
            let mut groups = groups
                .into_iter()
                .map(|(repo, mut pulls)| {
                    // Stable sort: open before draft, most recently updated first.
                    pulls.sort_by_key(|pr| pr.is_draft);
                    (repo, pulls)
                })
                .collect::<Vec<_>>();
            // Most recently active repository first; ties stay alphabetical.
            // GitHub timestamps are all UTC `…Z`, so they sort as strings.
            groups.sort_by_cached_key(|(_, pulls)| {
                Reverse(pulls.iter().map(|pr| pr.updated_at.clone()).max())
            });
            groups
        })
    });

    // j / k step through the pull requests in the order the tree shows them.
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        let typing = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA"));
        let step: isize = match ev.key().as_str() {
            "j" => 1,
            "k" => -1,
            _ => return,
        };
        if typing
            || ev.meta_key()
            || ev.ctrl_key()
            || ev.alt_key()
            || ctx.settings_open.get_untracked()
        {
            return;
        }
        let order: Vec<PullSummary> = groups.with_untracked(|groups| {
            folded_repos.with_untracked(|folded| {
                groups
                    .iter()
                    .filter(|(repo, _)| !folded.contains(repo))
                    .flat_map(|(_, pulls)| pulls.iter().cloned())
                    .collect()
            })
        });
        let current = ctx.selected.with_untracked(|s| {
            s.as_ref().and_then(|s| {
                order
                    .iter()
                    .position(|p| p.repo == s.repo && p.number == s.number)
            })
        });
        let next = match current {
            Some(i) => i
                .saturating_add_signed(step)
                .min(order.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(pr) = order.get(next) {
            ctx.selected.set(Some(pr.clone()));
            let id = row_id(pr);
            request_animation_frame(move || {
                if let Some(row) = document().get_element_by_id(&id) {
                    row.scroll_into_view_with_bool(false);
                }
            });
        }
    });
    on_cleanup(move || keys.remove());

    let pane = ctx.layout.sidebar;
    let total = move || inbox.with(|i| i.as_ref().map(|i| i.pulls.len().to_string()));
    let rail = move || {
        view! {
            <aside class="sidebar rail">
                <ResizeHandle pane/>
                <button
                    class="rail-button"
                    title="Show the pull requests"
                    on:click=move |_| pane.open.set(true)
                >
                    <span class="label">"Pull requests"</span>
                    <span class="count">{total}</span>
                </button>
            </aside>
        }
    };

    // Folding keeps this component, and with it the filters and j / k.
    view! {
        <Show when=move || pane.open.get() fallback=rail>
        <aside class="sidebar">
            <ResizeHandle pane/>
            <header class="pane-bar">
                <span class="label">"Pull requests"</span>
                <span class="count">{total}</span>
                <span class="spacer"></span>
                <button
                    class="icon-btn"
                    class:spinning=move || loading.get()
                    title="Refresh"
                    on:click=move |_| on_refresh.run(())
                >
                    "↻"
                </button>
                <button
                    class="icon-btn"
                    title="Settings"
                    on:click=move |_| ctx.settings_open.set(true)
                >
                    "⚙"
                </button>
                <button
                    class="icon-btn"
                    title="Hide this pane, to give the diff and code more room"
                    on:click=move |_| pane.open.set(false)
                >
                    "⇤"
                </button>
            </header>
            <div class="sidebar-filters">
                <input
                    class="search"
                    type="search"
                    placeholder="Filter: title, repo, #123, author"
                    prop:value=move || query.get()
                    on:input=move |ev| query.set(event_target_value(&ev))
                />
                <div class="segmented full">
                    {Filter::ALL
                        .into_iter()
                        .map(|f| {
                            view! {
                                <button
                                    class:active=move || filter.get() == f
                                    on:click=move |_| filter.set(f)
                                >
                                    {f.label()}
                                    <span class="count">{move || count(f)}</span>
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <label class="toggle">
                    <input
                        type="checkbox"
                        prop:checked=move || show_drafts.get()
                        on:change=move |ev| show_drafts.set(event_target_checked(&ev))
                    />
                    "Show drafts"
                </label>
            </div>
            {move || error.get().map(|e| view! { <div class="error-banner">{e}</div> })}
            <nav class="tree">
                {move || {
                    let groups = groups.get();
                    if groups.is_empty() {
                        let text = if inbox.with(Option::is_none) {
                            if loading.get() { "Loading…" } else { "Nothing loaded yet." }
                        } else {
                            "No pull requests match."
                        };
                        return view! { <p class="empty">{text}</p> }.into_any();
                    }
                    groups
                        .into_iter()
                        .map(|(repo, pulls)| view! { <RepoNode repo pulls folded=folded_repos/> })
                        .collect_view()
                        .into_any()
                }}
            </nav>
        </aside>
        </Show>
    }
}

#[component]
fn RepoNode(
    repo: String,
    pulls: Vec<PullSummary>,
    folded: RwSignal<HashSet<String>>,
) -> impl IntoView {
    let is_open = {
        let repo = repo.clone();
        move || !folded.with(|f| f.contains(&repo))
    };
    let toggle = {
        let repo = repo.clone();
        move |_| {
            folded.update(|f| {
                if !f.remove(&repo) {
                    f.insert(repo.clone());
                }
            })
        }
    };
    let (owner, name) = repo.split_once('/').unwrap_or(("", &repo));
    let (owner, name) = (format!("{owner}/"), name.to_string());
    let total = pulls.len();
    let first_draft = pulls.iter().position(|pr| pr.is_draft);

    view! {
        <div class="repo">
            <button class="repo-row" on:click=toggle title=repo.clone()>
                <span class="chevron" class:open=is_open.clone()>"▸"</span>
                <RepoIcon/>
                <span class="repo-name">
                    <span class="owner">{owner}</span>
                    {name}
                </span>
                <span class="count">{total}</span>
            </button>
            {move || {
                is_open()
                    .then(|| {
                        pulls
                            .iter()
                            .enumerate()
                            .map(|(i, pr)| {
                                let divider = (first_draft == Some(i) && i > 0)
                                    .then(|| view! { <div class="subgroup">"Drafts"</div> });
                                view! {
                                    {divider}
                                    <PullRow pr=pr.clone()/>
                                }
                            })
                            .collect_view()
                    })
            }}
        </div>
    }
}

fn row_id(pr: &PullSummary) -> String {
    format!("pr-{}-{}", pr.repo.replace('/', "-"), pr.number)
}

#[component]
fn PullRow(pr: PullSummary) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let is_selected = {
        let (repo, number) = (pr.repo.clone(), pr.number);
        move || {
            ctx.selected.with(|s| {
                s.as_ref()
                    .is_some_and(|s| s.repo == repo && s.number == number)
            })
        }
    };
    let select = {
        let pr = pr.clone();
        move |_| ctx.selected.set(Some(pr.clone()))
    };

    let roles = pr
        .roles
        .iter()
        .filter_map(|role| match role {
            Role::ReviewRequested => Some(("chip review", "review")),
            Role::Author => Some(("chip", "author")),
            Role::Assignee => Some(("chip", "assigned")),
            Role::Involved => None,
        })
        .map(|(class, label)| view! { <span class=class>{label}</span> })
        .collect_view();
    let decision = pr.review_decision.and_then(|d| match d {
        ReviewDecision::Approved => Some(view! { <span class="chip approved">"approved"</span> }),
        ReviewDecision::ChangesRequested => {
            Some(view! { <span class="chip changes">"changes"</span> })
        }
        ReviewDecision::ReviewRequired => None,
    });
    let checks = pr.checks.map(|c| {
        let (class, title) = match c {
            CheckState::Success => ("dot success", "Checks passed"),
            CheckState::Failure => ("dot failure", "Checks failed"),
            CheckState::Pending => ("dot pending", "Checks running"),
        };
        view! { <span class=class title=title></span> }
    });
    let author = pr.author.clone().map(|a| {
        let title = a.login.clone();
        view! {
            <span class="author">
                <Avatar url=a.avatar_url size=14/>
                <span class="login" title=title>{a.login}</span>
            </span>
        }
    });

    let sync = {
        let (repo, number) = (pr.repo.clone(), pr.number);
        move || {
            let (class, glyph, title) = match ctx.sync_status(&repo, number)? {
                SyncStatus::Queued => ("sync-glyph", "·", "Waiting to sync".to_string()),
                SyncStatus::Syncing { step } => ("sync-glyph busy", "◌", format!("{step}…")),
                SyncStatus::Ready => return None,
                SyncStatus::LocalChanges => (
                    "sync-glyph local",
                    "±",
                    "The worktree has changes of yours, so it was not updated".to_string(),
                ),
                SyncStatus::Stale { message } => (
                    "sync-glyph stale",
                    "~",
                    format!("Couldn't refresh, using the last synced worktree: {message}"),
                ),
                SyncStatus::Failed { message } => ("sync-glyph failed", "!", message),
            };
            Some(view! { <span class=class title=title>{glyph}</span> })
        }
    };

    view! {
        <button
            class="pr-row"
            id=row_id(&pr)
            class:selected=is_selected
            class:draft=pr.is_draft
            on:click=select
            title=pr.title.clone()
        >
            <PullIcon draft=pr.is_draft/>
            <div class="pr-main">
                <div class="pr-title">{pr.title.clone()}</div>
                <div class="pr-meta">
                    <span class="num">"#" {pr.number}</span>
                    <span class="time">{util::ago(&pr.updated_at)}</span>
                    {checks}
                    {decision}
                    {roles}
                    // Last, so a long login is what gets truncated.
                    {author}
                </div>
            </div>
            {sync}
        </button>
    }
}
