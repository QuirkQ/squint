use leptos::prelude::*;
use shared::{Comment, Review, ReviewState, ReviewThread, RunState, Side, TimelineItem};

use crate::actions::ChecksList;
use crate::app::{Ctx, Load, SideTab};
use crate::comments::{self, Composer};
use crate::layout::ResizeHandle;
use crate::util::{self, Avatar};

/// The right pane: the conversation, and the checks on the PR's head.
#[component]
pub fn Conversation() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let pane = ctx.layout.conversation;
    let count = move || {
        ctx.detail.with(|d| match d {
            Load::Ready(detail) => detail.timeline.len().to_string(),
            _ => String::new(),
        })
    };
    let checks_dot = move || {
        let class = match ctx.checks_rollup()? {
            RunState::Failure => "dot failure",
            RunState::Success => "dot success",
            _ => "dot pending",
        };
        Some(view! { <span class=class></span> })
    };
    let show = move |tab: SideTab| {
        ctx.side_tab.set(tab);
        pane.open.set(true);
    };
    let tab_button = move |tab: SideTab, label: &'static str, title: &'static str| {
        view! {
            <button
                class:active=move || ctx.side_tab.get() == tab
                title=title
                on:click=move |_| show(tab)
            >
                {label}
                {match tab {
                    SideTab::Conversation => view! { <span class="count">{count}</span> }.into_any(),
                    SideTab::Checks => checks_dot.into_any(),
                }}
            </button>
        }
    };
    let on_checks = move || ctx.side_tab.get() == SideTab::Checks;

    let collapsed = move || {
        view! {
            <aside class="conversation rail">
                <ResizeHandle pane/>
                <button
                    class="rail-button"
                    title="Show the conversation"
                    on:click=move |_| show(SideTab::Conversation)
                >
                    <span class="label">"Conversation"</span>
                    <span class="count">{count}</span>
                </button>
                <button
                    class="rail-button"
                    title="Show the checks"
                    on:click=move |_| show(SideTab::Checks)
                >
                    <span class="label">"Checks"</span>
                    {checks_dot}
                </button>
            </aside>
        }
    };

    view! {
        <Show when=move || pane.open.get() fallback=collapsed>
        <aside class="conversation">
            <ResizeHandle pane/>
            <header class="pane-bar">
                <div class="tabs">
                    {tab_button(SideTab::Conversation, "Conversation", "Description, comments and reviews")}
                    {tab_button(SideTab::Checks, "Checks", "Workflow runs and other checks on the PR's head; a job opens its log")}
                </div>
                <span class="spacer"></span>
                <button
                    class="icon-btn"
                    class:hidden=move || !on_checks()
                    class:spinning=move || ctx.actions.refreshing.get()
                    title="Refresh the checks"
                    on:click=move |_| ctx.refresh_checks()
                >
                    "↻"
                </button>
                <button
                    class="icon-btn"
                    title="Hide this pane, to give the diff and code more room"
                    on:click=move |_| pane.open.set(false)
                >
                    "⇥"
                </button>
            </header>
            <div class="checks" class:hidden=move || !on_checks()>
                <ChecksList/>
            </div>
            <div class="timeline" class:hidden=on_checks>
                {move || match ctx.detail.get() {
                    Load::Idle => ().into_any(),
                    Load::Loading => view! { <p class="loading">"Loading comments…"</p> }.into_any(),
                    Load::Failed(error) => view! { <div class="error-banner">{error}</div> }.into_any(),
                    Load::Ready(detail) => {
                        let items = detail
                            .timeline
                            .iter()
                            .enumerate()
                            .map(|(index, item)| match item {
                                TimelineItem::Comment(c) => comment_card(c, None).into_any(),
                                TimelineItem::Review(r) => review_card(r),
                                TimelineItem::Thread(t) => {
                                    view! { <ThreadCard index thread=t.clone()/> }.into_any()
                                }
                            })
                            .collect_view();
                        view! {
                            {comment_card(&detail.description, Some(("chip", "opened")))}
                            {items}
                        }
                            .into_any()
                    }
                }}
            </div>
        </aside>
        </Show>
    }
}

fn comment_header(
    comment: &Comment,
    badge: Option<(&'static str, &'static str)>,
) -> impl IntoView + use<> {
    let (login, avatar) = match &comment.author {
        Some(a) => (a.login.clone(), Some(a.avatar_url.clone())),
        None => ("ghost".to_string(), None),
    };
    view! {
        <header class="comment-head">
            {avatar.map(|url| view! { <Avatar url size=20/> })}
            <strong>{login}</strong>
            {badge.map(|(class, text)| view! { <span class=class>{text}</span> })}
            <a class="time" href=comment.url.clone() title=comment.created_at.clone()>
                {util::ago(&comment.created_at)}
            </a>
        </header>
    }
}

fn comment_card(
    comment: &Comment,
    badge: Option<(&'static str, &'static str)>,
) -> impl IntoView + use<> {
    let body = if comment.body_html.trim().is_empty() {
        view! { <p class="muted">"No description provided."</p> }.into_any()
    } else {
        // Rendered and sanitised by GitHub; the CSP blocks any script in it.
        view! { <div class="markdown-body" inner_html=comment.body_html.clone()></div> }.into_any()
    };
    view! {
        <article class="comment">
            {comment_header(comment, badge)}
            {body}
        </article>
    }
}

fn review_card(review: &Review) -> AnyView {
    let badge = match review.state {
        ReviewState::Approved => ("chip approved", "approved"),
        ReviewState::ChangesRequested => ("chip changes", "requested changes"),
        ReviewState::Commented => ("chip", "reviewed"),
        ReviewState::Dismissed => ("chip", "dismissed review"),
        ReviewState::Pending => ("chip", "pending review"),
    };
    let comment = Comment {
        author: review.author.clone(),
        body_html: review.body_html.clone(),
        created_at: review.submitted_at.clone(),
        url: review.url.clone(),
    };
    if review.body_html.trim().is_empty() {
        // A bare verdict reads better as a one-line event.
        view! { <div class="event">{comment_header(&comment, Some(badge))}</div> }.into_any()
    } else {
        comment_card(&comment, Some(badge)).into_any()
    }
}

#[component]
fn ThreadCard(index: usize, thread: ReviewThread) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let expanded = RwSignal::new(!thread.is_resolved);
    let location = match (thread.start_line, thread.line) {
        (Some(start), Some(line)) => format!("{}:{start}–{line}", thread.path),
        (None, Some(line)) => format!("{}:{line}", thread.path),
        _ => thread.path.clone(),
    };
    // Long paths lose their directory to the ellipsis, never the file name.
    let (dir, file) = match location.rsplit_once('/') {
        Some((dir, file)) => (format!("{dir}/"), file.to_string()),
        None => (String::new(), location.clone()),
    };
    let jump = {
        let path = thread.path.clone();
        let anchor = thread.line.map(|line| (thread.side, line));
        move |_| ctx.show_location(&path, anchor)
    };
    let side = (thread.side == Side::Old).then(|| view! { <span class="chip">"base"</span> });
    let count = thread.comments.len();
    let comments = thread.comments.clone();
    let foot = StoredValue::new(thread.clone());

    view! {
        <section class="thread" class:resolved=thread.is_resolved id=format!("thread-{index}")>
            <header class="thread-head">
                <button class="link path" on:click=jump title=format!("Show {location}")>
                    <span class="path-dir">{dir}</span>
                    <span class="path-file">{file}</span>
                </button>
                {side}
                {thread.is_outdated.then(|| view! { <span class="chip">"outdated"</span> })}
                {thread.is_resolved.then(|| view! { <span class="chip approved">"resolved"</span> })}
                <button class="link small" on:click=move |_| expanded.update(|e| *e = !*e)>
                    {move || {
                        if expanded.get() {
                            "Hide".to_string()
                        } else {
                            format!("Show {count}")
                        }
                    }}
                </button>
            </header>
            {move || {
                expanded
                    .get()
                    .then(|| {
                        view! {
                            {comments.iter().map(|c| comment_card(c, None)).collect_view()}
                            <ThreadFoot thread=foot.get_value()/>
                        }
                    })
            }}
        </section>
    }
}

/// Replying to a thread, and resolving it, as far as GitHub lets you.
#[component]
fn ThreadFoot(thread: ReviewThread) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let busy = RwSignal::new(false);
    let id = StoredValue::new(thread.id);
    let reply =
        Memo::new(move |_| id.with_value(|id| ctx.comments.replies.with(|r| r.get(id).cloned())));
    let root = thread.root_comment_id.filter(|_| thread.can_reply);
    let resolved = thread.is_resolved;
    let can_toggle = if resolved {
        thread.can_unresolve
    } else {
        thread.can_resolve
    };
    if root.is_none() && !can_toggle {
        return ().into_any();
    }

    let resolve_button = move || {
        can_toggle.then(|| {
            view! {
                <button
                    class="btn small"
                    disabled=move || busy.get()
                    on:click=move |_| ctx.set_resolved(id.get_value(), !resolved, busy)
                >
                    {move || match (busy.get(), resolved) {
                        (false, false) => "Resolve",
                        (false, true) => "Unresolve",
                        (true, false) => "Resolving…",
                        (true, true) => "Unresolving…",
                    }}
                </button>
            }
        })
    };
    view! {
        <footer class="thread-foot">
            {move || match (reply.get(), root) {
                (Some(text), Some(root)) => view! {
                    <Composer
                        id=id.with_value(|id| comments::reply_box_id(id))
                        text
                        placeholder="Reply…"
                        send_label="Reply"
                        on_send=Callback::new(move |()| ctx.send_reply(id.get_value(), root))
                        on_cancel=Callback::new(move |()| id.with_value(|id| ctx.close_reply(id)))
                        suggestion=None
                    />
                }
                .into_any(),
                _ => view! {
                    {root.map(|_| view! {
                        <button class="reply-prompt" on:click=move |_| ctx.open_reply(id.get_value())>
                            "Reply…"
                        </button>
                    })}
                    {resolve_button()}
                }
                .into_any(),
            }}
        </footer>
    }
    .into_any()
}
