use std::collections::{HashMap, HashSet};
use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use shared::{PullSummary, Role};

use crate::api;
use crate::app::{Ctx, Load};

type Key = (String, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Approve,
    Close,
}

/// Under the diff and the conversation: approve or close the selected pull
/// request, with an optional comment. `on_done` refreshes the inbox.
#[component]
pub fn ReviewBar(on_done: Callback<()>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // Unsent comments, so looking at another PR on the way keeps them.
    let drafts = StoredValue::new(HashMap::<Key, String>::new());
    // A closed PR stays selected after the inbox drops it.
    let closed = RwSignal::new(HashSet::<Key>::new());
    let pr = Memo::new(move |_| ctx.selected.get());
    move || pr.get().map(|pr| view! { <Bar pr drafts closed on_done/> })
}

#[component]
fn Bar(
    pr: PullSummary,
    drafts: StoredValue<HashMap<Key, String>>,
    closed: RwSignal<HashSet<Key>>,
    on_done: Callback<()>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let key = StoredValue::new((pr.repo.clone(), pr.number));
    let comment = RwSignal::new(
        key.with_value(|key| drafts.with_value(|d| d.get(key).cloned().unwrap_or_default())),
    );
    let busy = RwSignal::new(None::<Verdict>);
    let confirming = RwSignal::new(false);
    let mine = pr.roles.contains(&Role::Author);
    let head = StoredValue::new(pr.head_sha);
    let input = NodeRef::<html::Textarea>::new();

    // The box grows with the comment, up to its max-height.
    Effect::new(move |_| {
        comment.track();
        if let Some(el) = input.get_untracked() {
            let style = web_sys::HtmlElement::style(&el);
            let _ = style.set_property("height", "auto");
            let _ = style.set_property("height", &format!("{}px", el.scroll_height() + 2));
        }
    });

    let submit = move |verdict: Verdict| {
        if verdict == Verdict::Close && !confirming.get_untracked() {
            confirming.set(true);
            set_timeout(move || confirming.set(false), Duration::from_secs(4));
            return;
        }
        confirming.set(false);
        busy.set(Some(verdict));
        let (repo, number) = key.get_value();
        let text = comment.get_untracked();
        // Approve the head the diff showed, not one pushed since.
        let commit = ctx.detail.with_untracked(|d| match d {
            Load::Ready(detail) => detail.head_sha.clone(),
            _ => head.get_value(),
        });
        spawn_local(async move {
            let result = match verdict {
                Verdict::Approve => api::approve_pull(&repo, number, &commit, &text).await,
                Verdict::Close => api::close_pull(&repo, number, &text).await,
            };
            match result {
                Ok(()) => {
                    let done = match verdict {
                        Verdict::Approve => "Approved",
                        Verdict::Close => "Closed",
                    };
                    ctx.notify(format!("{done} {repo}#{number}"), false);
                    drafts.update_value(|d| {
                        d.remove(&(repo.clone(), number));
                    });
                    comment.set(String::new());
                    if verdict == Verdict::Close {
                        closed.update(|c| {
                            c.insert((repo.clone(), number));
                        });
                    }
                    ctx.reload_detail(repo, number);
                    on_done.run(());
                }
                Err(error) => ctx.notify(error, true),
            }
            busy.set(None);
        });
    };

    let is_closed = move || key.with_value(|key| closed.with(|c| c.contains(key)));
    let approve_title = if mine {
        "GitHub doesn't let you approve your own pull request"
    } else {
        "Approve the PR as the diff shows it, with the comment if there is one"
    };
    let url = pr.url;
    let closed_note = move || {
        view! {
            <footer class="review-bar closed">
                <span class="chip">"closed"</span>
                <span class="note">"Closed without merging."</span>
                <a class="btn" href=url.clone()>"Reopen on GitHub"</a>
            </footer>
        }
    };

    view! {
        <Show when=move || !is_closed() fallback=closed_note>
            <footer class="review-bar">
                <textarea
                    node_ref=input
                    rows="1"
                    placeholder="Leave a comment (optional)"
                    prop:value=move || comment.get()
                    readonly=move || busy.get().is_some()
                    on:input=move |ev| {
                        let text = event_target_value(&ev);
                        key.with_value(|key| {
                            drafts.update_value(|d| {
                                d.insert(key.clone(), text.clone());
                            })
                        });
                        comment.set(text);
                    }
                ></textarea>
                <button
                    class="btn primary"
                    disabled=move || mine || busy.get().is_some()
                    title=approve_title
                    on:click=move |_| submit(Verdict::Approve)
                >
                    {move || match busy.get() {
                        Some(Verdict::Approve) => "Approving…",
                        _ => "Approve",
                    }}
                </button>
                <button
                    class="btn danger"
                    class:confirming=move || confirming.get()
                    disabled=move || busy.get().is_some()
                    title="Close the PR without merging; the comment, if any, is posted with it"
                    on:click=move |_| submit(Verdict::Close)
                >
                    {move || match (busy.get(), confirming.get()) {
                        (Some(Verdict::Close), _) => "Closing…",
                        (_, true) => "Click again to close",
                        _ => "Close PR",
                    }}
                </button>
            </footer>
        </Show>
    }
}
