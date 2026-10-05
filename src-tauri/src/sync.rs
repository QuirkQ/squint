//! Keeps the review workspace in step with the inbox.
//!
//! Every inbox refresh asks for a sync *round*. A round fetches each
//! repository once (three at a time), moves its pull requests' worktrees to
//! their new heads, and, when the inbox was complete, cleans up what is gone.
//! A refresh that arrives mid-round is folded into one follow-up round.
//! Selecting a pull request jumps the queue through [`Engine::ensure`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use shared::{
    Inbox, KeptWorktree, PullSummary, Role, RoundEvent, SyncEvent, SyncStatus, WorkspaceSummary,
};
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::git::Git;
use crate::workspace::{Outcome, Removal, Workspace};

const PARALLEL_REPOS: usize = 3;

type Key = (String, u32);

fn key(pr: &PullSummary) -> Key {
    (pr.repo.clone(), pr.number)
}

/// Shared sync state, kept in the app state for the app's lifetime.
#[derive(Default)]
pub struct SyncState {
    statuses: Mutex<HashMap<Key, SyncStatus>>,
    /// The inbox head each worktree was last synced to, to skip unchanged PRs.
    synced: Mutex<HashMap<Key, String>>,
    /// The latest inbox's pull requests, so `ensure` knows what to sync.
    pulls: Mutex<HashMap<Key, PullSummary>>,
    kept: Mutex<Vec<KeptWorktree>>,
    /// Serialises git work per repository: rounds, `ensure` and clean-up.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    rounds: Mutex<Rounds>,
}

#[derive(Default)]
struct Rounds {
    running: bool,
    pending: Option<(Inbox, Git)>,
}

impl SyncState {
    pub fn summary(&self, workspace: &Workspace) -> WorkspaceSummary {
        let statuses = self
            .statuses
            .lock()
            .unwrap()
            .iter()
            .map(|((repo, number), status)| SyncEvent {
                repo: repo.clone(),
                number: *number,
                status: status.clone(),
            })
            .collect();
        WorkspaceSummary {
            root: workspace.root().to_string_lossy().into_owned(),
            round_running: self.rounds.lock().unwrap().running,
            statuses,
            kept: self.kept.lock().unwrap().clone(),
        }
    }
}

/// What the engine reports; the app forwards these as Tauri events.
pub enum Notice {
    Sync(SyncEvent),
    Round(RoundEvent),
}

pub type Notifier = Arc<dyn Fn(Notice) + Send + Sync>;

#[derive(Clone)]
pub struct Engine {
    notify: Notifier,
    state: Arc<SyncState>,
    workspace: Arc<Workspace>,
    git: Git,
}

impl Engine {
    pub fn new(
        notify: Notifier,
        state: Arc<SyncState>,
        workspace: Arc<Workspace>,
        git: Git,
    ) -> Self {
        Engine {
            notify,
            state,
            workspace,
            git,
        }
    }

    pub fn request_round(&self, inbox: Inbox) {
        *self.state.pulls.lock().unwrap() =
            inbox.pulls.iter().map(|pr| (key(pr), pr.clone())).collect();
        let mut rounds = self.state.rounds.lock().unwrap();
        rounds.pending = Some((inbox, self.git.clone()));
        if rounds.running {
            return;
        }
        rounds.running = true;
        (self.notify)(Notice::Round(RoundEvent { running: true }));
        drop(rounds);

        let engine = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let next = {
                    let mut rounds = engine.state.rounds.lock().unwrap();
                    let next = rounds.pending.take();
                    if next.is_none() {
                        rounds.running = false;
                        (engine.notify)(Notice::Round(RoundEvent { running: false }));
                    }
                    next
                };
                let Some((inbox, git)) = next else { break };
                engine.round(inbox, git).await;
            }
        });
    }

    async fn round(&self, inbox: Inbox, git: Git) {
        let mut repos: HashMap<String, Vec<PullSummary>> = HashMap::new();
        for pr in &inbox.pulls {
            repos.entry(pr.repo.clone()).or_default().push(pr.clone());
        }
        let mut repos: Vec<_> = repos.into_iter().collect();
        // Repos with reviews waiting on you first, then your own, then the rest.
        repos.sort_by_key(|(_, prs)| {
            let rank = prs.iter().map(priority).min().unwrap_or(u8::MAX);
            let newest = prs
                .iter()
                .map(|pr| pr.updated_at.clone())
                .max()
                .unwrap_or_default();
            (rank, std::cmp::Reverse(newest))
        });

        let semaphore = Arc::new(Semaphore::new(PARALLEL_REPOS));
        let mut tasks = tokio::task::JoinSet::new();
        for (repo, prs) in repos {
            let stale: Vec<PullSummary> =
                prs.into_iter().filter(|pr| !self.is_current(pr)).collect();
            if stale.is_empty() {
                continue;
            }
            for pr in &stale {
                self.set(key(pr), SyncStatus::Queued);
            }
            let (engine, git, semaphore) = (self.clone(), git.clone(), semaphore.clone());
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                let lock = engine.lock(&repo);
                let _guard = lock.lock().await;
                // `ensure` may have synced some of these while we queued.
                let stale: Vec<&PullSummary> =
                    stale.iter().filter(|pr| !engine.is_current(pr)).collect();
                if !stale.is_empty() {
                    engine.sync_repo(&git, &repo, &stale).await;
                }
            });
        }
        while tasks.join_next().await.is_some() {}

        if inbox.complete {
            self.clean_up(&git, &inbox).await;
        }
    }

    /// Syncs one pull request right now and returns its worktree, plus a
    /// warning when your changes kept it from moving.
    pub async fn ensure(&self, repo: &str, number: u32) -> Result<(PathBuf, Option<String>)> {
        let key = (repo.to_string(), number);
        let pr = self.pull(repo, number)?;
        if !self.is_current(&pr) {
            let lock = self.lock(repo);
            let _guard = lock.lock().await;
            if !self.is_current(&pr) {
                self.sync_repo(&self.git, repo, &[&pr]).await;
            }
        }
        let worktree = self.workspace.worktree_dir(repo, number)?;
        match self.state.statuses.lock().unwrap().get(&key) {
            Some(SyncStatus::Ready) => Ok((worktree, None)),
            Some(SyncStatus::LocalChanges) => Ok((
                worktree,
                Some(
                    "The worktree has changes of yours, so it was left on its previous commit."
                        .into(),
                ),
            )),
            Some(SyncStatus::Stale { message }) => Ok((
                worktree,
                Some(format!(
                    "Couldn't refresh the worktree, so it's the last synced version: {message}"
                )),
            )),
            Some(SyncStatus::Failed { message }) => Err(Error::msg(message.clone())),
            _ => Err(Error::msg("The worktree is not ready yet.")),
        }
    }

    /// *Delete anyway* for a kept worktree.
    pub async fn remove_kept(&self, repo: &str, number: u32) -> Result<()> {
        let lock = self.lock(repo);
        let _guard = lock.lock().await;
        self.workspace.remove(&self.git, repo, number).await?;
        let still_open = self
            .state
            .pulls
            .lock()
            .unwrap()
            .keys()
            .any(|(r, _)| r == repo);
        if !still_open {
            self.workspace.remove_repo_if_empty(repo).await?;
        }
        self.state
            .kept
            .lock()
            .unwrap()
            .retain(|k| !(k.repo == repo && k.number == number));
        Ok(())
    }

    /// Clones or fetches a repository once, then updates each pull request's
    /// worktree. The caller holds the repository's lock.
    async fn sync_repo(&self, git: &Git, repo: &str, prs: &[&PullSummary]) {
        let cloned = self
            .workspace
            .repo_dir(repo)
            .is_ok_and(|dir| dir.join(".bare").exists());
        let step = if cloned { "fetching" } else { "cloning" };
        for pr in prs {
            self.set(key(pr), SyncStatus::Syncing { step: step.into() });
        }

        let fetched = async {
            self.workspace.ensure_repo(git, repo).await?;
            self.workspace.fetch(git, repo, prs).await
        };
        let previous = match fetched.await {
            Ok(previous) => previous,
            Err(error) => {
                for pr in prs {
                    self.set(key(pr), self.failure(pr, &error));
                }
                return;
            }
        };

        for pr in prs {
            self.set(
                key(pr),
                SyncStatus::Syncing {
                    step: "checking out".into(),
                },
            );
            let previous = previous.get(&pr.number).map(String::as_str);
            let status = match self.workspace.sync_worktree(git, pr, previous).await {
                Ok((Outcome::Ready, head)) => {
                    // The commit actually checked out: GitHub's refs/pull can
                    // trail a push by a moment, and then the next round must
                    // still see this PR as behind the inbox.
                    self.state.synced.lock().unwrap().insert(key(pr), head);
                    SyncStatus::Ready
                }
                Ok((Outcome::LocalChanges, _)) => SyncStatus::LocalChanges,
                Err(error) => self.failure(pr, &error),
            };
            self.set(key(pr), status);
        }
    }

    /// A failed refresh only fails the PR when there's no worktree to fall
    /// back on; otherwise it's stale, still usable, and retried next round.
    fn failure(&self, pr: &PullSummary, error: &Error) -> SyncStatus {
        let message = error.to_string();
        let usable = self
            .workspace
            .worktree_dir(&pr.repo, pr.number)
            .is_ok_and(|dir| dir.join(".git").exists());
        if usable {
            SyncStatus::Stale { message }
        } else {
            SyncStatus::Failed { message }
        }
    }

    /// *Update anyway* for a worktree your changes hold back.
    pub async fn force_update(&self, repo: &str, number: u32) -> Result<String> {
        let pr = self.pull(repo, number)?;
        let lock = self.lock(repo);
        let _guard = lock.lock().await;
        let message = self.workspace.force_update(&self.git, &pr).await?;
        self.state.synced.lock().unwrap().remove(&key(&pr));
        self.set(key(&pr), SyncStatus::Ready);
        Ok(message)
    }

    fn pull(&self, repo: &str, number: u32) -> Result<PullSummary> {
        self.state
            .pulls
            .lock()
            .unwrap()
            .get(&(repo.to_string(), number))
            .cloned()
            .ok_or_else(|| {
                Error::msg("This pull request is not in the inbox. Refresh and try again.")
            })
    }

    /// Removes worktrees of pull requests that left the inbox, and
    /// repositories left without any. Only called for complete inboxes.
    async fn clean_up(&self, git: &Git, inbox: &Inbox) {
        let open: HashSet<Key> = inbox.pulls.iter().map(key).collect();
        let open_repos: HashSet<&str> = inbox.pulls.iter().map(|pr| pr.repo.as_str()).collect();
        let mut kept = Vec::new();

        for (repo, number) in self.workspace.worktrees() {
            let key = (repo.clone(), number);
            if open.contains(&key) {
                continue;
            }
            let lock = self.lock(&repo);
            let _guard = lock.lock().await;
            let reason = match self.workspace.remove_if_unused(git, &repo, number).await {
                Ok(Removal::Removed) => {
                    self.state.statuses.lock().unwrap().remove(&key);
                    self.state.synced.lock().unwrap().remove(&key);
                    continue;
                }
                Ok(Removal::Kept(reason)) => reason,
                Err(error) => error.to_string(),
            };
            let path = self
                .workspace
                .worktree_dir(&repo, number)
                .unwrap_or_default();
            kept.push(KeptWorktree {
                repo,
                number,
                path: path.to_string_lossy().into_owned(),
                reason,
            });
        }

        for (repo, _) in self.workspace.repos() {
            if !open_repos.contains(repo.as_str()) {
                let lock = self.lock(&repo);
                let _guard = lock.lock().await;
                let _ = self.workspace.remove_repo_if_empty(&repo).await;
            }
        }
        *self.state.kept.lock().unwrap() = kept;
    }

    /// Up to date: synced to the inbox's head and still on disk.
    fn is_current(&self, pr: &PullSummary) -> bool {
        let key = key(pr);
        let ready = matches!(
            self.state.statuses.lock().unwrap().get(&key),
            Some(SyncStatus::Ready)
        );
        let synced = self.state.synced.lock().unwrap().get(&key) == Some(&pr.head_sha);
        ready
            && synced
            && !pr.head_sha.is_empty()
            && self
                .workspace
                .worktree_dir(&pr.repo, pr.number)
                .is_ok_and(|dir| dir.join(".git").exists())
    }

    fn set(&self, key: Key, status: SyncStatus) {
        self.state
            .statuses
            .lock()
            .unwrap()
            .insert(key.clone(), status.clone());
        let event = SyncEvent {
            repo: key.0,
            number: key.1,
            status,
        };
        (self.notify)(Notice::Sync(event));
    }

    fn lock(&self, repo: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.state
            .locks
            .lock()
            .unwrap()
            .entry(repo.to_string())
            .or_default()
            .clone()
    }
}

fn priority(pr: &PullSummary) -> u8 {
    if pr.has_role(Role::ReviewRequested) {
        0
    } else if pr.has_role(Role::Author) || pr.has_role(Role::Assignee) {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::Fixture;

    fn engine(fx: &Fixture) -> (Engine, Arc<Mutex<Vec<SyncEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let notify: Notifier = Arc::new(move |notice| {
            if let Notice::Sync(event) = notice {
                sink.lock().unwrap().push(event);
            }
        });
        let engine = Engine::new(notify, Arc::default(), Arc::new(fx.workspace()), fx.git());
        (engine, events)
    }

    fn inbox(pulls: &[&PullSummary], complete: bool) -> Inbox {
        Inbox {
            viewer: "me".into(),
            pulls: pulls.iter().map(|pr| (*pr).clone()).collect(),
            complete,
        }
    }

    /// Two PRs on their own branches, with the inbox's head shas filled in.
    fn two_prs(fx: &Fixture) -> (PullSummary, PullSummary) {
        let mut prs = Vec::new();
        for (number, branch) in [(1, "feat/a"), (2, "feat/b")] {
            fx.sh(&fx.src(), &["checkout", "--quiet", "-B", branch, "main"]);
            let head = fx.commit(&format!("{number}.txt"), "x\n");
            fx.publish(branch, number);
            let mut pr = fx.pr(number, branch, false);
            pr.head_sha = head;
            prs.push(pr);
        }
        let second = prs.pop().unwrap();
        (prs.pop().unwrap(), second)
    }

    fn status(engine: &Engine, number: u32) -> Option<SyncStatus> {
        engine
            .state
            .statuses
            .lock()
            .unwrap()
            .get(&("o/r".to_string(), number))
            .cloned()
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    #[test]
    fn rounds_sync_and_only_complete_inboxes_clean_up() {
        let fx = Fixture::new();
        let (one, two) = two_prs(&fx);
        let (engine, events) = engine(&fx);
        let ws = fx.workspace();

        block_on(engine.round(inbox(&[&one, &two], true), fx.git()));
        assert_eq!(status(&engine, 1), Some(SyncStatus::Ready));
        assert_eq!(status(&engine, 2), Some(SyncStatus::Ready));
        let steps: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.number == 1)
            .map(|e| e.status.clone())
            .collect();
        assert_eq!(
            steps,
            [
                SyncStatus::Queued,
                SyncStatus::Syncing {
                    step: "cloning".into()
                },
                SyncStatus::Syncing {
                    step: "checking out".into()
                },
                SyncStatus::Ready,
            ]
        );

        // A partial inbox that misses #2 must not delete it.
        block_on(engine.round(inbox(&[&one], false), fx.git()));
        assert!(ws.worktree_dir("o/r", 2).unwrap().exists());

        // A complete one that misses it does.
        block_on(engine.round(inbox(&[&one], true), fx.git()));
        assert!(!ws.worktree_dir("o/r", 2).unwrap().exists());
        assert!(ws.worktree_dir("o/r", 1).unwrap().exists());

        // And once the repo has no PRs left, so does the clone.
        block_on(engine.round(inbox(&[], true), fx.git()));
        assert!(!ws.repo_dir("o/r").unwrap().exists());
    }

    #[test]
    fn unchanged_prs_are_not_touched_again() {
        let fx = Fixture::new();
        let (one, two) = two_prs(&fx);
        let (engine, events) = engine(&fx);
        block_on(engine.round(inbox(&[&one, &two], true), fx.git()));
        events.lock().unwrap().clear();

        block_on(engine.round(inbox(&[&one, &two], true), fx.git()));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn ensure_and_a_round_share_the_repo_safely() {
        let fx = Fixture::new();
        let (one, two) = two_prs(&fx);
        let (engine, _) = engine(&fx);
        let both = inbox(&[&one, &two], true);
        *engine.state.pulls.lock().unwrap() =
            both.pulls.iter().map(|pr| (key(pr), pr.clone())).collect();

        let (_, ensured) = block_on(async {
            tokio::join!(
                engine.round(both.clone(), fx.git()),
                engine.ensure("o/r", 2)
            )
        });
        let (worktree, warning) = ensured.unwrap();
        assert_eq!(worktree, fx.workspace().worktree_dir("o/r", 2).unwrap());
        assert_eq!(warning, None);
        assert_eq!(status(&engine, 1), Some(SyncStatus::Ready));
        let list = fx.sh(
            &fx.workspace().repo_dir("o/r").unwrap(),
            &["worktree", "list"],
        );
        assert_eq!(
            list.lines().count(),
            3,
            "the bare repo plus two worktrees:\n{list}"
        );
    }

    #[test]
    fn ensure_needs_the_pr_in_the_inbox() {
        let fx = Fixture::new();
        let (engine, _) = engine(&fx);
        assert!(block_on(engine.ensure("o/r", 9)).is_err());
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_worktree_usable() {
        let fx = Fixture::new();
        let (one, _) = two_prs(&fx);
        let (engine, _) = engine(&fx);
        block_on(engine.round(inbox(&[&one], true), fx.git()));

        // GitHub becomes unreachable just as the PR moves on.
        std::fs::rename(fx.remote(), fx.remote().with_extension("gone")).unwrap();
        let mut moved = one.clone();
        moved.head_sha = "f".repeat(40);
        *engine.state.pulls.lock().unwrap() = [(key(&moved), moved.clone())].into();
        block_on(engine.round(inbox(&[&moved], true), fx.git()));
        assert!(matches!(status(&engine, 1), Some(SyncStatus::Stale { .. })));

        let (worktree, warning) = block_on(engine.ensure("o/r", 1)).unwrap();
        assert!(worktree.join(".git").exists());
        assert!(warning.unwrap().starts_with("Couldn't refresh"));
    }

    #[test]
    fn a_pull_ref_that_trails_the_inbox_is_synced_again() {
        let fx = Fixture::new();
        let (mut one, _) = two_prs(&fx);
        // The inbox already knows a newer head than refs/pull has caught up to.
        one.head_sha = "f".repeat(40);
        let (engine, events) = engine(&fx);
        block_on(engine.round(inbox(&[&one], true), fx.git()));
        assert_eq!(status(&engine, 1), Some(SyncStatus::Ready));
        events.lock().unwrap().clear();

        block_on(engine.round(inbox(&[&one], true), fx.git()));
        assert!(!events.lock().unwrap().is_empty());
    }
}
