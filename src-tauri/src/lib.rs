mod code;
mod credentials;
mod error;
mod git;
mod github;
mod highlight;
mod shell_env;
mod sync;
mod workspace;
mod zed;

use std::sync::Arc;

use shared::{
    Checks, CodeTree, DiffShas, FileContent, FileDiff, GrepQuery, GrepResult, Inbox, Job,
    LineHighlights, NewComment, PullDetail, TokenSource, WorkspaceSummary, ZedLaunch,
};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{Mutex, OnceCell};

use error::{Error, Result};
use git::Git;
use github::GitHub;
use shell_env::ShellEnv;
use sync::{Engine, Notice, SyncState};
use workspace::Workspace;

#[derive(Default)]
struct AppState {
    env: OnceCell<ShellEnv>,
    // Not a OnceCell: saving or removing a token in Settings replaces it.
    github: Mutex<Option<(GitHub, TokenSource)>>,
    workspace: OnceCell<Arc<Workspace>>,
    sync: Arc<SyncState>,
}

impl AppState {
    async fn env(&self) -> &ShellEnv {
        self.env.get_or_init(ShellEnv::capture).await
    }

    async fn workspace(&self) -> Result<Arc<Workspace>> {
        let env = self.env().await;
        self.workspace
            .get_or_try_init(|| async { Workspace::from_env(env).map(Arc::new) })
            .await
            .cloned()
    }

    /// Git for the review workspace, with the token for fetching.
    async fn git(&self, app: &AppHandle) -> Result<Git> {
        let github = self.github(app).await?;
        Ok(Git::new(self.env().await, github.token()))
    }

    async fn engine(&self, app: &AppHandle) -> Result<Engine> {
        let git = self.git(app).await?;
        let app = app.clone();
        let notify = Arc::new(move |notice| {
            let _ = match notice {
                Notice::Sync(event) => app.emit("workspace-sync", event),
                Notice::Round(event) => app.emit("workspace-round", event),
            };
        });
        Ok(Engine::new(
            notify,
            self.sync.clone(),
            self.workspace().await?,
            git,
        ))
    }

    async fn github(&self, app: &AppHandle) -> Result<GitHub> {
        self.session(app)
            .await?
            .map(|(client, _)| client)
            .ok_or_else(|| Error::msg("Add a GitHub token in Settings to get started."))
    }

    /// The client for the token in use, and where the token came from. Kept
    /// once found: macOS can ask for the login password on every Keychain
    /// read, so the token is read once per launch.
    async fn session(&self, app: &AppHandle) -> Result<Option<(GitHub, TokenSource)>> {
        let mut github = self.github.lock().await;
        if github.is_none() {
            *github = match find_token(app)? {
                Some((token, source)) => Some((GitHub::new(token)?, source)),
                None => None,
            };
        }
        Ok(github.clone())
    }
}

/// Environment variables win, as they do for `gh`, so a token can be
/// overridden for one run without touching the Keychain.
fn find_token(app: &AppHandle) -> Result<Option<(String, TokenSource)>> {
    if let Some(token) = env_token() {
        return Ok(Some((token, TokenSource::Environment)));
    }
    Ok(credentials::load(keychain_service(app))?.map(|t| (t, TokenSource::Keychain)))
}

fn env_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

fn keychain_service(app: &AppHandle) -> &str {
    &app.config().identifier
}

#[tauri::command]
async fn token_source(app: AppHandle, state: State<'_, AppState>) -> Result<Option<TokenSource>> {
    Ok(state.session(&app).await?.map(|(_, source)| source))
}

/// Checks the token against GitHub before storing it, so a typo never
/// replaces a working token. Returns the login it belongs to.
#[tauri::command]
async fn save_token(app: AppHandle, state: State<'_, AppState>, token: String) -> Result<String> {
    let client = GitHub::new(token.trim().to_string())?;
    let login = client.viewer_login().await?;
    credentials::save(keychain_service(&app), client.token())?;
    // A token from the environment still wins, and stays in use.
    if env_token().is_none() {
        *state.github.lock().await = Some((client, TokenSource::Keychain));
    }
    Ok(login)
}

#[tauri::command]
async fn delete_token(app: AppHandle, state: State<'_, AppState>) -> Result<()> {
    credentials::delete(keychain_service(&app))?;
    *state.github.lock().await = None;
    Ok(())
}

#[tauri::command]
async fn inbox(app: AppHandle, state: State<'_, AppState>) -> Result<Inbox> {
    let inbox = state.github(&app).await?.inbox().await?;
    state.engine(&app).await?.request_round(inbox.clone());
    Ok(inbox)
}

#[tauri::command]
async fn pull_detail(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<PullDetail> {
    state.github(&app).await?.detail(&repo, number).await
}

#[tauri::command]
async fn pull_files(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<Vec<FileDiff>> {
    state.github(&app).await?.files(&repo, number).await
}

#[tauri::command]
async fn pull_checks(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<Checks> {
    state.github(&app).await?.checks(&repo, number).await
}

#[tauri::command]
async fn run_jobs(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    run: u64,
) -> Result<Vec<Job>> {
    state.github(&app).await?.run_jobs(&repo, run).await
}

#[tauri::command]
async fn job_log(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    job: u64,
) -> Result<String> {
    state.github(&app).await?.job_log(&repo, job).await
}

/// Approves the pull request at `commit`, the head the diff showed.
#[tauri::command]
async fn approve_pull(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
    commit: String,
    comment: String,
) -> Result<()> {
    state
        .github(&app)
        .await?
        .approve(&repo, number, &commit, &comment)
        .await
}

#[tauri::command]
async fn close_pull(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
    comment: String,
) -> Result<()> {
    state
        .github(&app)
        .await?
        .close(&repo, number, &comment)
        .await
}

#[tauri::command]
async fn add_comment(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
    comment: NewComment,
) -> Result<()> {
    state
        .github(&app)
        .await?
        .add_comment(&repo, number, &comment)
        .await
}

/// Replies to a review thread; `comment` is the REST id of its first comment.
#[tauri::command]
async fn reply_to_thread(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
    comment: u64,
    body: String,
) -> Result<()> {
    state
        .github(&app)
        .await?
        .reply(&repo, number, comment, &body)
        .await
}

#[tauri::command]
async fn resolve_thread(
    app: AppHandle,
    state: State<'_, AppState>,
    thread: String,
    resolved: bool,
) -> Result<()> {
    state
        .github(&app)
        .await?
        .set_resolved(&thread, resolved)
        .await
}

#[tauri::command]
async fn open_in_zed(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
    path: Option<String>,
    line: Option<u32>,
) -> Result<ZedLaunch> {
    if let Some(path) = &path {
        zed::check_path(path)?;
    }
    let (worktree, warning) = state.engine(&app).await?.ensure(&repo, number).await?;
    zed::launch(state.env().await, &worktree, path.as_deref(), line).await?;
    Ok(ZedLaunch {
        worktree: worktree.to_string_lossy().into_owned(),
        warning,
    })
}

/// Syncs the selected pull request ahead of the round; progress arrives as
/// `workspace-sync` events.
#[tauri::command]
async fn ensure_pr(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<()> {
    state
        .engine(&app)
        .await?
        .ensure(&repo, number)
        .await
        .map(|_| ())
}

#[tauri::command]
async fn workspace_status(state: State<'_, AppState>) -> Result<WorkspaceSummary> {
    Ok(state.sync.summary(&*state.workspace().await?))
}

/// Bytes on disk, via `du`, which is far faster than walking the tree here.
#[tauri::command]
async fn workspace_disk_usage(state: State<'_, AppState>) -> Result<u64> {
    let root = state.workspace().await?.root().to_path_buf();
    if !root.exists() {
        return Ok(0);
    }
    let output = tokio::process::Command::new("/usr/bin/du")
        .arg("-sk")
        .arg(&root)
        .output()
        .await?;
    let kilobytes: u64 = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|kb| kb.parse().ok())
        .unwrap_or(0);
    Ok(kilobytes * 1024)
}

#[tauri::command]
async fn reveal_workspace(app: AppHandle, state: State<'_, AppState>) -> Result<()> {
    let root = state.workspace().await?.root().to_path_buf();
    std::fs::create_dir_all(&root)?;
    app.opener()
        .reveal_item_in_dir(root)
        .map_err(|e| Error::msg(e.to_string()))
}

/// *Update anyway* for a worktree your changes hold back; says where your
/// work was saved.
#[tauri::command]
async fn update_worktree(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<String> {
    state.engine(&app).await?.force_update(&repo, number).await
}

/// *Delete anyway* for a worktree kept because it holds your work.
#[tauri::command]
async fn remove_worktree(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<()> {
    state.engine(&app).await?.remove_kept(&repo, number).await
}

/// The files at a pull request's head, for the Code tab.
#[tauri::command]
async fn code_tree(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
) -> Result<CodeTree> {
    let git = state.git(&app).await?;
    code::tree(&git, &*state.workspace().await?, &repo, number).await
}

#[tauri::command]
async fn read_file(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    sha: String,
    path: String,
) -> Result<FileContent> {
    let git = state.git(&app).await?;
    code::read_file(&git, &*state.workspace().await?, &repo, &sha, &path).await
}

#[tauri::command]
async fn grep(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    sha: String,
    query: GrepQuery,
) -> Result<GrepResult> {
    let git = state.git(&app).await?;
    code::grep(&git, &*state.workspace().await?, &repo, &sha, &query).await
}

/// The commits the diff compares, for highlighting it; `paths` are the old
/// paths of the changed files.
#[tauri::command]
async fn diff_shas(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    number: u32,
    base: String,
    paths: Vec<String>,
) -> Result<DiffShas> {
    let git = state.git(&app).await?;
    code::diff_shas(
        &git,
        &*state.workspace().await?,
        &repo,
        number,
        &base,
        &paths,
    )
    .await
}

#[tauri::command]
async fn highlight_lines(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
    sha: String,
    path: String,
    ranges: Vec<(u32, u32)>,
) -> Result<LineHighlights> {
    let git = state.git(&app).await?;
    code::highlight_lines(
        &git,
        &*state.workspace().await?,
        &repo,
        &sha,
        &path,
        &ranges,
    )
    .await
}

/// Links inside comments open in the browser rather than inside the app window.
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<()> {
    let url = match url.strip_prefix('/') {
        Some(path) => format!("https://github.com/{path}"),
        None => url,
    };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Error::msg(format!("Not opening {url}")));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| Error::msg(e.to_string()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            token_source,
            save_token,
            delete_token,
            inbox,
            pull_detail,
            pull_files,
            pull_checks,
            run_jobs,
            job_log,
            approve_pull,
            close_pull,
            open_in_zed,
            ensure_pr,
            workspace_status,
            workspace_disk_usage,
            reveal_workspace,
            remove_worktree,
            update_worktree,
            code_tree,
            read_file,
            grep,
            diff_shas,
            highlight_lines,
            open_url,
            add_comment,
            reply_to_thread,
            resolve_thread
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
