//! Typed wrappers around the backend's Tauri commands.

use serde::Serialize;
use serde::de::DeserializeOwned;
use shared::{
    Checks, CodeTree, DiffShas, FileContent, FileDiff, GrepQuery, GrepResult, Inbox, Job,
    LineHighlights, NewComment, PullDetail, TokenSource, WorkspaceSummary, ZedLaunch,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], catch)]
    async fn listen(event: &str, handler: &js_sys::Function) -> Result<JsValue, JsValue>;
}

/// Calls `handler` with each payload of a backend event, for the app's lifetime.
pub fn on_event<T: DeserializeOwned + 'static>(event: &'static str, handler: impl Fn(T) + 'static) {
    let closure = Closure::<dyn Fn(JsValue)>::new(move |message: JsValue| {
        let payload = js_sys::Reflect::get(&message, &"payload".into()).unwrap_or(JsValue::NULL);
        match serde_wasm_bindgen::from_value(payload) {
            Ok(value) => handler(value),
            Err(error) => leptos::logging::warn!("{event}: {error}"),
        }
    });
    let function: js_sys::Function = closure.as_ref().unchecked_ref::<js_sys::Function>().clone();
    // The listener lives as long as the app, so the closure must too.
    closure.forget();
    leptos::task::spawn_local(async move {
        if let Err(error) = listen(event, &function).await {
            leptos::logging::warn!("listen {event}: {error:?}");
        }
    });
}

async fn call<R: DeserializeOwned>(cmd: &str, args: impl Serialize) -> Result<R, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    match invoke(cmd, args).await {
        Ok(value) => serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string()),
        // Commands reject with the backend error's message.
        Err(error) => Err(error.as_string().unwrap_or_else(|| format!("{error:?}"))),
    }
}

#[derive(Serialize)]
struct NoArgs {}

#[derive(Serialize)]
struct PullArgs<'a> {
    repo: &'a str,
    number: u32,
}

#[derive(Serialize)]
struct ZedArgs<'a> {
    repo: &'a str,
    number: u32,
    path: Option<&'a str>,
    line: Option<u32>,
}

pub async fn token_source() -> Result<Option<TokenSource>, String> {
    call("token_source", NoArgs {}).await
}

pub async fn save_token(token: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        token: &'a str,
    }
    call("save_token", Args { token }).await
}

pub async fn delete_token() -> Result<(), String> {
    call("delete_token", NoArgs {}).await
}

pub async fn inbox() -> Result<Inbox, String> {
    call("inbox", NoArgs {}).await
}

pub async fn pull_detail(repo: &str, number: u32) -> Result<PullDetail, String> {
    call("pull_detail", PullArgs { repo, number }).await
}

pub async fn pull_files(repo: &str, number: u32) -> Result<Vec<FileDiff>, String> {
    call("pull_files", PullArgs { repo, number }).await
}

pub async fn pull_checks(repo: &str, number: u32) -> Result<Checks, String> {
    call("pull_checks", PullArgs { repo, number }).await
}

pub async fn run_jobs(repo: &str, run: u64) -> Result<Vec<Job>, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        run: u64,
    }
    call("run_jobs", Args { repo, run }).await
}

pub async fn job_log(repo: &str, job: u64) -> Result<String, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        job: u64,
    }
    call("job_log", Args { repo, job }).await
}

pub async fn approve_pull(
    repo: &str,
    number: u32,
    commit: &str,
    comment: &str,
) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        number: u32,
        commit: &'a str,
        comment: &'a str,
    }
    let args = Args {
        repo,
        number,
        commit,
        comment,
    };
    call("approve_pull", args).await
}

pub async fn close_pull(repo: &str, number: u32, comment: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        number: u32,
        comment: &'a str,
    }
    call(
        "close_pull",
        Args {
            repo,
            number,
            comment,
        },
    )
    .await
}

pub async fn add_comment(repo: &str, number: u32, comment: &NewComment) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        number: u32,
        comment: &'a NewComment,
    }
    let args = Args {
        repo,
        number,
        comment,
    };
    call("add_comment", args).await
}

/// `comment` is the REST id of the thread's first comment.
pub async fn reply_to_thread(
    repo: &str,
    number: u32,
    comment: u64,
    body: &str,
) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        number: u32,
        comment: u64,
        body: &'a str,
    }
    let args = Args {
        repo,
        number,
        comment,
        body,
    };
    call("reply_to_thread", args).await
}

pub async fn resolve_thread(thread: &str, resolved: bool) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args<'a> {
        thread: &'a str,
        resolved: bool,
    }
    call("resolve_thread", Args { thread, resolved }).await
}

pub async fn open_in_zed(
    repo: &str,
    number: u32,
    path: Option<&str>,
    line: Option<u32>,
) -> Result<ZedLaunch, String> {
    let args = ZedArgs {
        repo,
        number,
        path,
        line,
    };
    call("open_in_zed", args).await
}

pub async fn ensure_pr(repo: &str, number: u32) -> Result<(), String> {
    call("ensure_pr", PullArgs { repo, number }).await
}

pub async fn workspace_status() -> Result<WorkspaceSummary, String> {
    call("workspace_status", NoArgs {}).await
}

pub async fn workspace_disk_usage() -> Result<u64, String> {
    call("workspace_disk_usage", NoArgs {}).await
}

pub async fn reveal_workspace() -> Result<(), String> {
    call("reveal_workspace", NoArgs {}).await
}

pub async fn update_worktree(repo: &str, number: u32) -> Result<String, String> {
    call("update_worktree", PullArgs { repo, number }).await
}

pub async fn remove_worktree(repo: &str, number: u32) -> Result<(), String> {
    call("remove_worktree", PullArgs { repo, number }).await
}

pub async fn code_tree(repo: &str, number: u32) -> Result<CodeTree, String> {
    call("code_tree", PullArgs { repo, number }).await
}

pub async fn read_file(repo: &str, sha: &str, path: &str) -> Result<FileContent, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        sha: &'a str,
        path: &'a str,
    }
    call("read_file", Args { repo, sha, path }).await
}

pub async fn grep(repo: &str, sha: &str, query: &GrepQuery) -> Result<GrepResult, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        sha: &'a str,
        query: &'a GrepQuery,
    }
    call("grep", Args { repo, sha, query }).await
}

pub async fn diff_shas(
    repo: &str,
    number: u32,
    base: &str,
    paths: &[String],
) -> Result<DiffShas, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        number: u32,
        base: &'a str,
        paths: &'a [String],
    }
    call(
        "diff_shas",
        Args {
            repo,
            number,
            base,
            paths,
        },
    )
    .await
}

pub async fn highlight_lines(
    repo: &str,
    sha: &str,
    path: &str,
    ranges: &[(u32, u32)],
) -> Result<LineHighlights, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        repo: &'a str,
        sha: &'a str,
        path: &'a str,
        ranges: &'a [(u32, u32)],
    }
    call(
        "highlight_lines",
        Args {
            repo,
            sha,
            path,
            ranges,
        },
    )
    .await
}

pub async fn open_url(url: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args<'a> {
        url: &'a str,
    }
    call("open_url", Args { url }).await
}
