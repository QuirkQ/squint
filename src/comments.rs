//! Review comments written in the app: new ones on a line, or a range of
//! lines, of the diff, and replies to the conversation's threads. Drafts live
//! here rather than in the views, so re-rendering a diff or the conversation
//! never loses what you typed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use leptos::ev::{FocusEvent, KeyboardEvent};
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use shared::{Anchor, FileDiff, NewComment, Side, TimelineItem};

use crate::api;
use crate::app::{Ctx, Load};
use crate::util::{self, Icon};

/// A line of a file's diff: its hunk, and its index in that hunk's lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub hunk: usize,
    pub index: usize,
}

impl Spot {
    /// The `data-spot` attribute that tells the pointer which line it's on
    /// while picking lines.
    pub fn attr(self) -> String {
        format!("{}:{}", self.hunk, self.index)
    }

    pub fn parse(attr: &str) -> Option<Spot> {
        let (hunk, index) = attr.split_once(':')?;
        Some(Spot {
            hunk: hunk.parse().ok()?,
            index: index.parse().ok()?,
        })
    }
}

/// Lines of one hunk to highlight, and in the split view which columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub hunk: usize,
    pub first: usize,
    pub last: usize,
    pub old: bool,
    pub new: bool,
}

impl Span {
    /// `column` is the split view's, or `None` for a unified row.
    pub fn covers(&self, spot: Spot, column: Option<Side>) -> bool {
        spot.hunk == self.hunk
            && (self.first..=self.last).contains(&spot.index)
            && match column {
                None => true,
                Some(Side::Old) => self.old,
                Some(Side::New) => self.new,
            }
    }
}

/// Lines being picked for a new comment, while the mouse button is down:
/// from the line whose + was pressed to the one under the pointer.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub path: String,
    /// The split view's column the pick started in, and stays in.
    pub column: Option<Side>,
    pub from: Spot,
    pub to: Spot,
}

impl Pick {
    pub fn span(&self) -> Span {
        Span {
            hunk: self.from.hunk,
            first: self.from.index.min(self.to.index),
            last: self.from.index.max(self.to.index),
            old: self.column != Some(Side::New),
            new: self.column != Some(Side::Old),
        }
    }
}

/// What a comment box edits, and how sending it goes.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Text {
    pub body: ArcRwSignal<String>,
    pub sending: ArcRwSignal<bool>,
    pub error: ArcRwSignal<Option<String>>,
}

/// A new comment on a line, or a range of lines, being written.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub id: u32,
    /// The pull request's repo and number.
    pub pr: (String, u32),
    /// The head commit the diff showed.
    pub commit: String,
    pub path: String,
    pub start: Anchor,
    pub end: Anchor,
    /// The head's version of the lines, which *Suggest* starts from; `None`
    /// unless the comment is on the head side.
    pub suggestion: Option<String>,
    pub text: Text,
}

impl Draft {
    /// "Comment on line +4", "Comment on lines −3 to +4".
    pub fn heading(&self) -> String {
        let at = |a: Anchor| match a.side {
            Side::Old => format!("−{}", a.line),
            Side::New => format!("+{}", a.line),
        };
        if self.start == self.end {
            format!("Comment on line {}", at(self.end))
        } else {
            format!("Comment on lines {} to {}", at(self.start), at(self.end))
        }
    }
}

/// Where a draft sits in the diff on show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub id: u32,
    /// The line its comment box follows.
    pub after: Spot,
    pub end_side: Side,
    /// The lines it's on; `None` once the diff no longer shows them, e.g.
    /// after a push, and the box waits at the end of the file instead.
    pub span: Option<Span>,
}

#[derive(Clone, Copy)]
pub struct CommentsState {
    pub pick: RwSignal<Option<Pick>>,
    pub drafts: RwSignal<Vec<Draft>>,
    /// Replies being written, by thread id.
    pub replies: RwSignal<HashMap<String, Text>>,
    next_id: StoredValue<u32>,
    /// The comment box to focus as soon as it renders.
    focus_next: StoredValue<Option<String>>,
    /// The comment box being typed in, which gets the focus back when the
    /// diff around it renders again (a new layout, highlighting arriving).
    focused: StoredValue<Option<String>>,
}

impl Default for CommentsState {
    fn default() -> Self {
        CommentsState {
            pick: RwSignal::new(None),
            drafts: RwSignal::new(Vec::new()),
            replies: RwSignal::new(HashMap::new()),
            next_id: StoredValue::new(0),
            focus_next: StoredValue::new(None),
            focused: StoredValue::new(None),
        }
    }
}

impl CommentsState {
    pub fn draft(&self, id: u32) -> Option<Draft> {
        self.drafts
            .with_untracked(|drafts| drafts.iter().find(|d| d.id == id).cloned())
    }
}

/// A pick ends wherever the button comes up, even outside the diff.
pub fn setup(ctx: Ctx) {
    let ups = window_event_listener(leptos::ev::mouseup, move |_| {
        if let Some(pick) = ctx.comments.pick.get_untracked() {
            ctx.comments.pick.set(None);
            ctx.open_draft(pick);
        }
    });
    on_cleanup(move || ups.remove());
}

pub fn draft_box_id(id: u32) -> String {
    format!("draft-{id}")
}

pub fn reply_box_id(thread: &str) -> String {
    format!("reply-{thread}")
}

impl Ctx {
    fn shown_pr(&self) -> Option<(String, u32)> {
        self.selected
            .with_untracked(|pr| pr.as_ref().map(|pr| (pr.repo.clone(), pr.number)))
    }

    /// Starts picking lines at the + pressed on `spot`.
    pub fn start_pick(&self, path: String, column: Option<Side>, spot: Spot) {
        self.comments.pick.set(Some(Pick {
            path,
            column,
            from: spot,
            to: spot,
        }));
    }

    /// Stretches the pick to the line under the pointer, within the hunk
    /// and the column it started in: GitHub can't place a range that skips
    /// lines the diff doesn't show.
    pub fn extend_pick(&self, path: &str, column: Option<Side>, spot: Spot) {
        let fits = self.comments.pick.with_untracked(|pick| {
            pick.as_ref().is_some_and(|p| {
                p.path == path && p.column == column && p.from.hunk == spot.hunk && p.to != spot
            })
        });
        if fits {
            self.comments.pick.update(|pick| {
                if let Some(pick) = pick {
                    pick.to = spot;
                }
            });
        }
    }

    /// Opens a comment box on the picked lines, or goes back to the one
    /// already open on them.
    fn open_draft(&self, pick: Pick) {
        let Some(pr) = self.shown_pr() else {
            return;
        };
        let span = pick.span();
        let found = self.files.with_untracked(|files| {
            let Load::Ready(files) = files else {
                return None;
            };
            let hunk = files
                .iter()
                .find(|f| f.path == pick.path)?
                .hunks
                .get(span.hunk)?;
            let start = hunk.anchor(span.first, pick.column)?;
            let end = hunk.anchor(span.last, pick.column)?;
            let on_head = start.side == Side::New && end.side == Side::New;
            let suggestion = on_head.then(|| hunk.head_text(span.first, span.last));
            Some((start, end, suggestion))
        });
        let Some((start, end, suggestion)) = found else {
            return;
        };
        let same =
            |d: &Draft| d.pr == pr && d.path == pick.path && d.start == start && d.end == end;
        if let Some(open) = self
            .comments
            .drafts
            .with_untracked(|d| d.iter().find(|d| same(d)).map(|d| d.id))
        {
            self.focus_box(draft_box_id(open));
            return;
        }
        // The diff and the conversation are fetched together, so the
        // conversation's head is the one the diff shows.
        let commit = self.detail.with_untracked(|d| match d {
            Load::Ready(detail) => Some(detail.head_sha.clone()),
            _ => None,
        });
        let Some(commit) = commit.or_else(|| {
            self.selected
                .with_untracked(|pr| pr.as_ref().map(|pr| pr.head_sha.clone()))
        }) else {
            return;
        };
        let id = self.comments.next_id.get_value();
        self.comments.next_id.set_value(id + 1);
        self.comments.focus_next.set_value(Some(draft_box_id(id)));
        self.comments.drafts.update(|drafts| {
            drafts.push(Draft {
                id,
                pr,
                commit,
                path: pick.path,
                start,
                end,
                suggestion,
                text: Text::default(),
            })
        });
    }

    /// Where the selected PR's drafts on `file` sit in its diff.
    pub fn placed_drafts(&self, file: &FileDiff) -> Vec<Placed> {
        let pr = self
            .selected
            .with(|pr| pr.as_ref().map(|pr| (pr.repo.clone(), pr.number)));
        let file_end = file.hunks.len().checked_sub(1).map(|hunk| Spot {
            hunk,
            index: file.hunks[hunk].lines.len().saturating_sub(1),
        });
        self.comments.drafts.with(|drafts| {
            drafts
                .iter()
                .filter(|d| Some(&d.pr) == pr.as_ref() && d.path == file.path)
                .filter_map(|d| {
                    let span = file.locate(d.end).map(|(hunk, last)| {
                        let first = file
                            .locate(d.start)
                            .filter(|&(h, _)| h == hunk)
                            .map_or(last, |(_, first)| first.min(last));
                        Span {
                            hunk,
                            first,
                            last,
                            old: d.start.side == Side::Old || d.end.side == Side::Old,
                            new: d.start.side == Side::New || d.end.side == Side::New,
                        }
                    });
                    let after = match span {
                        Some(span) => Spot {
                            hunk: span.hunk,
                            index: span.last,
                        },
                        None => file_end?,
                    };
                    Some(Placed {
                        id: d.id,
                        after,
                        end_side: d.end.side,
                        span,
                    })
                })
                .collect()
        })
    }

    pub fn post_draft(&self, id: u32) {
        let Some(draft) = self.comments.draft(id) else {
            return;
        };
        let text = draft.text.clone();
        let body = text.body.get_untracked();
        if body.trim().is_empty() || text.sending.get_untracked() {
            return;
        }
        text.sending.set(true);
        text.error.set(None);
        let ctx = *self;
        spawn_local(async move {
            let (repo, number) = draft.pr;
            let comment = NewComment {
                commit: draft.commit,
                path: draft.path,
                start: draft.start,
                end: draft.end,
                body: body.trim_end().to_string(),
            };
            match api::add_comment(&repo, number, &comment).await {
                Ok(()) => {
                    ctx.comments.drafts.update(|d| d.retain(|d| d.id != id));
                    ctx.forget_box(&draft_box_id(id));
                    ctx.notify("Comment posted", false);
                    ctx.reload_detail(repo, number);
                }
                Err(error) => {
                    text.sending.set(false);
                    text.error.set(Some(error));
                }
            }
        });
    }

    pub fn discard_draft(&self, id: u32) {
        self.comments.drafts.update(|d| d.retain(|d| d.id != id));
        self.forget_box(&draft_box_id(id));
    }

    pub fn open_reply(&self, thread: String) {
        self.comments
            .focus_next
            .set_value(Some(reply_box_id(&thread)));
        self.comments.replies.update(|replies| {
            replies.entry(thread).or_default();
        });
    }

    pub fn close_reply(&self, thread: &str) {
        self.comments.replies.update(|replies| {
            replies.remove(thread);
        });
        self.forget_box(&reply_box_id(thread));
    }

    /// Replies to a thread; `comment` is the REST id of its first comment.
    pub fn send_reply(&self, thread: String, comment: u64) {
        let Some(text) = self
            .comments
            .replies
            .with_untracked(|replies| replies.get(&thread).cloned())
        else {
            return;
        };
        let Some((repo, number)) = self.shown_pr() else {
            return;
        };
        let body = text.body.get_untracked();
        if body.trim().is_empty() || text.sending.get_untracked() {
            return;
        }
        text.sending.set(true);
        text.error.set(None);
        let ctx = *self;
        spawn_local(async move {
            match api::reply_to_thread(&repo, number, comment, body.trim_end()).await {
                Ok(()) => {
                    ctx.close_reply(&thread);
                    ctx.reload_detail(repo, number);
                }
                Err(error) => {
                    text.sending.set(false);
                    text.error.set(Some(error));
                }
            }
        });
    }

    /// Resolves a thread, or unresolves it. The conversation shows the new
    /// state at once, then reloads to be sure.
    pub fn set_resolved(&self, thread: String, resolved: bool, busy: RwSignal<bool>) {
        let Some((repo, number)) = self.shown_pr() else {
            return;
        };
        busy.set(true);
        let ctx = *self;
        spawn_local(async move {
            match api::resolve_thread(&thread, resolved).await {
                Ok(()) => {
                    ctx.detail.update(|detail| {
                        let Load::Ready(detail) = detail else {
                            return;
                        };
                        for item in &mut Arc::make_mut(detail).timeline {
                            if let TimelineItem::Thread(t) = item
                                && t.id == thread
                            {
                                t.is_resolved = resolved;
                                t.can_resolve = !resolved;
                                t.can_unresolve = resolved;
                            }
                        }
                    });
                    ctx.reload_detail(repo, number);
                }
                Err(error) => ctx.notify(error, true),
            }
            // Showing the new state renders the thread anew, button and all.
            busy.try_set(false);
        });
    }

    fn focus_box(&self, id: String) {
        if !util::focus_text(&id) {
            self.comments.focus_next.set_value(Some(id));
        }
    }

    fn forget_box(&self, id: &str) {
        for slot in [self.comments.focused, self.comments.focus_next] {
            if slot.with_value(|f| f.as_deref() == Some(id)) {
                slot.set_value(None);
            }
        }
    }
}

/// How many lines the box shows: what's typed, within reason.
fn rows(body: &str) -> usize {
    body.split('\n').count().clamp(3, 16)
}

/// A comment box. ⌘↩ sends; *Cancel* asks again before throwing away text.
#[component]
pub fn Composer(
    /// The textarea's id, which also tracks its focus across re-renders.
    id: String,
    text: Text,
    placeholder: &'static str,
    send_label: &'static str,
    on_send: Callback<()>,
    on_cancel: Callback<()>,
    /// Starts *Suggest*'s block, when the comment is on the head's lines.
    suggestion: Option<String>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.comments;
    let body = RwSignal::from(text.body);
    let sending = RwSignal::from(text.sending);
    let error = RwSignal::from(text.error);
    let confirming = RwSignal::new(false);
    let input = NodeRef::<html::Textarea>::new();
    let id = StoredValue::new(id);

    // A new box takes the focus; one rendered again takes it back if it had
    // it and nothing else has it now.
    Effect::new(move |_| {
        let Some(element) = input.get() else {
            return;
        };
        let mine = |slot: StoredValue<Option<String>>| {
            id.with_value(|id| slot.with_value(|f| f.as_ref() == Some(id)))
        };
        let restoring = mine(state.focused) && util::nothing_focused();
        if !mine(state.focus_next) && !restoring {
            return;
        }
        state.focus_next.set_value(None);
        // A new box is where you just clicked; one coming back mustn't drag
        // the diff to wherever it is.
        let options = web_sys::FocusOptions::new();
        options.set_prevent_scroll(restoring);
        let _ = element.focus_with_options(&options);
        let end = element.value().len() as u32;
        let _ = element.set_selection_range(end, end);
    });

    let send = move || {
        if !sending.get_untracked() && !body.with_untracked(|b| b.trim().is_empty()) {
            on_send.run(());
        }
    };
    let cancel = move || {
        if body.with_untracked(|b| b.trim().is_empty()) || confirming.get_untracked() {
            confirming.set(false);
            on_cancel.run(());
            return;
        }
        confirming.set(true);
        // The box may be gone by then.
        set_timeout(
            move || {
                confirming.try_set(false);
            },
            Duration::from_secs(4),
        );
    };
    let on_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "Enter" if ev.meta_key() || ev.ctrl_key() => {
            ev.prevent_default();
            send();
        }
        "Escape" if body.with_untracked(|b| b.trim().is_empty()) => {
            ev.prevent_default();
            on_cancel.run(());
        }
        _ => {}
    };
    let on_focus = move |_| state.focused.set_value(Some(id.get_value()));
    // Focus moving elsewhere lets go; the box being removed or the window
    // losing focus doesn't.
    let on_blur = move |ev: FocusEvent| {
        if ev.related_target().is_some()
            && id.with_value(|id| state.focused.with_value(|f| f.as_ref() == Some(id)))
        {
            state.focused.set_value(None);
        }
    };
    let suggest = suggestion.map(|lines| {
        let block = format!("```suggestion\n{lines}\n```\n");
        view! {
            <button
                class="btn small"
                title="Suggest a change to these lines, which the author can commit from GitHub"
                disabled=move || sending.get()
                on:click=move |_| {
                    body.update(|b| {
                        if !b.is_empty() && !b.ends_with('\n') {
                            b.push('\n');
                        }
                        b.push_str(&block);
                    });
                    if let Some(element) = input.get_untracked() {
                        let _ = element.focus();
                    }
                }
            >
                <Icon path=util::DIFF_ICON size=12/>
                "Suggest"
            </button>
        }
    });

    view! {
        <div class="composer">
            <textarea
                id=id.get_value()
                node_ref=input
                placeholder=placeholder
                rows=move || body.with(|b| rows(b))
                prop:value=move || body.get()
                readonly=move || sending.get()
                on:input=move |ev| body.set(event_target_value(&ev))
                on:keydown=on_key
                on:focus=on_focus
                on:blur=on_blur
            ></textarea>
            {move || error.get().map(|error| view! { <p class="error-text">{error}</p> })}
            <div class="composer-actions">
                {suggest}
                <span class="hint">"Markdown · ⌘↩ to send"</span>
                <span class="spacer"></span>
                <button class="btn small" disabled=move || sending.get() on:click=move |_| cancel()>
                    {move || if confirming.get() { "Click again to discard" } else { "Cancel" }}
                </button>
                <button
                    class="btn small primary"
                    disabled=move || sending.get() || body.with(|b| b.trim().is_empty())
                    on:click=move |_| send()
                >
                    {move || if sending.get() { "Sending…" } else { send_label }}
                </button>
            </div>
        </div>
    }
}
