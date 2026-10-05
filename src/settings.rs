use leptos::ev::{self, SubmitEvent};
use leptos::prelude::*;
use leptos::task::spawn_local;
use shared::{SyncStatus, TokenSource};

use crate::app::Ctx;
use crate::{api, util};

const NEW_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo,read:org&description=PR%20Review";

#[component]
pub fn Settings(on_saved: Callback<()>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let source = LocalResource::new(api::token_source);
    let token = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<Result<String, String>>);
    let close = move || ctx.settings_open.set(false);

    let escape = window_event_listener(ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            close();
        }
    });
    on_cleanup(move || escape.remove());

    let save = move |ev: SubmitEvent| {
        ev.prevent_default();
        let value = token.get_untracked();
        if value.trim().is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        message.set(None);
        spawn_local(async move {
            match api::save_token(&value).await {
                Ok(login) => {
                    token.set(String::new());
                    message.set(Some(Ok(format!("Saved. Signed in as @{login}."))));
                    source.refetch();
                    on_saved.run(());
                }
                Err(error) => message.set(Some(Err(error))),
            }
            busy.set(false);
        });
    };
    let remove = move |_| {
        spawn_local(async move {
            match api::delete_token().await {
                Ok(()) => message.set(Some(Ok("Removed the token from the Keychain.".into()))),
                Err(error) => message.set(Some(Err(error))),
            }
            source.refetch();
        });
    };

    let status = move || {
        let text = match source.get() {
            None => "Checking…",
            Some(Ok(Some(TokenSource::Keychain))) => "A token is stored in your Keychain.",
            Some(Ok(Some(TokenSource::Environment))) => {
                "Using GH_TOKEN / GITHUB_TOKEN from the environment, which takes precedence over the Keychain."
            }
            Some(Ok(None)) => "No token yet. Paste one below to get started.",
            Some(Err(_)) => "Could not read the Keychain.",
        };
        view! { <p>{text}</p> }
    };
    let in_keychain = move || matches!(source.get(), Some(Ok(Some(TokenSource::Keychain))));

    view! {
        <div class="modal-backdrop" on:click=move |_| close()>
            <div class="modal" role="dialog" aria-label="Settings" on:click=|ev| ev.stop_propagation()>
                <header class="modal-head">
                    <h2>"Settings"</h2>
                    <button class="icon-btn" title="Close" on:click=move |_| close()>
                        "✕"
                    </button>
                </header>
                <section>
                    <h3>"GitHub token"</h3>
                    {status}
                    <form class="token-form" on:submit=save>
                        <input
                            type="password"
                            placeholder="ghp_… or github_pat_…"
                            autocomplete="off"
                            spellcheck="false"
                            prop:value=move || token.get()
                            on:input=move |ev| token.set(event_target_value(&ev))
                        />
                        <button class="btn primary" type="submit" disabled=move || busy.get()>
                            {move || if busy.get() { "Checking…" } else { "Save" }}
                        </button>
                    </form>
                    {move || {
                        message
                            .get()
                            .map(|m| match m {
                                Ok(text) => view! { <p class="success">{text}</p> }.into_any(),
                                Err(text) => view! { <p class="error-text">{text}</p> }.into_any(),
                            })
                    }}
                    <p class="hint">
                        "The token is checked against GitHub before it is stored. A classic token needs the "
                        <code>"repo"</code> " and " <code>"read:org"</code>
                        " scopes. If your organisation uses SAML single sign-on, authorise the token for it afterwards."
                    </p>
                    <div class="modal-actions">
                        <a class="btn" href=NEW_TOKEN_URL>
                            "Create a token on GitHub ↗"
                        </a>
                        <Show when=in_keychain>
                            <button class="btn danger" on:click=remove>
                                "Remove from Keychain"
                            </button>
                        </Show>
                    </div>
                </section>
                <Workspace refresh=on_saved/>
            </div>
        </div>
    }
}

/// Where the review workspace lives, what it costs, and anything kept back.
#[component]
fn Workspace(refresh: Callback<()>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let summary = LocalResource::new(api::workspace_status);
    let disk = LocalResource::new(api::workspace_disk_usage);
    let error = RwSignal::new(None::<String>);

    let reveal = move |_| {
        spawn_local(async move {
            if let Err(e) = api::reveal_workspace().await {
                error.set(Some(e));
            }
        })
    };
    let delete = move |repo: String, number: u32| {
        spawn_local(async move {
            match api::remove_worktree(&repo, number).await {
                Ok(()) => {
                    summary.refetch();
                    disk.refetch();
                }
                Err(e) => error.set(Some(e)),
            }
        })
    };
    let worktrees = move || {
        ctx.sync.with(|s| {
            s.values()
                .filter(|status| matches!(status, SyncStatus::Ready | SyncStatus::LocalChanges))
                .count()
        })
    };

    view! {
        <section>
            <h3>"Review workspace"</h3>
            <p class="hint">
                "A fresh clone of every repository in your inbox and a worktree per open PR, synced on every refresh. Worktrees of merged or closed PRs are removed. Your own clones are never touched."
            </p>
            <div class="workspace-path">
                <code>
                    {move || {
                        summary.get().and_then(Result::ok).map(|s| util::tilde(&s.root)).unwrap_or_default()
                    }}
                </code>
                <button class="btn small" on:click=reveal>
                    "Reveal in Finder"
                </button>
            </div>
            <p class="workspace-stats">
                {move || disk.get().and_then(Result::ok).map(util::bytes).unwrap_or_else(|| "…".into())}
                " on disk · "
                {worktrees}
                " worktrees"
                {move || ctx.syncing.get().then_some(" · syncing")}
            </p>
            {move || {
                let kept = summary.get().and_then(Result::ok).map(|s| s.kept).unwrap_or_default();
                (!kept.is_empty())
                    .then(|| {
                        let rows = kept
                            .into_iter()
                            .map(|k| {
                                let (repo, number) = (k.repo.clone(), k.number);
                                view! {
                                    <li>
                                        <span class="kept-name">{k.repo.clone()} " #" {k.number}</span>
                                        <span class="kept-reason">{k.reason.clone()}</span>
                                        <button
                                            class="btn small danger"
                                            title=k.path.clone()
                                            on:click=move |_| delete(repo.clone(), number)
                                        >
                                            "Delete anyway"
                                        </button>
                                    </li>
                                }
                            })
                            .collect_view();
                        view! {
                            <p class="hint">"Kept because they hold your work, though the PR is gone:"</p>
                            <ul class="kept">{rows}</ul>
                        }
                    })
            }}
            {move || error.get().map(|e| view! { <p class="error-text">{e}</p> })}
            <div class="modal-actions">
                <button
                    class="btn"
                    disabled=move || ctx.syncing.get()
                    on:click=move |_| {
                        refresh.run(());
                        summary.refetch();
                    }
                >
                    "Sync now"
                </button>
            </div>
        </section>
    }
}
