//! The review workspace: one blobless bare clone per repository and one
//! worktree per open pull request, all owned by the app.
//!
//! ```text
//! <root>/github.com/<owner>/<repo>/.bare      the clone
//! <root>/github.com/<owner>/<repo>/.git       "gitdir: ./.bare"
//! <root>/github.com/<owner>/<repo>/pr-<n>     a worktree on the PR's branch
//! ```
//!
//! The directory tree is the only index. Nothing here ever touches the clones
//! you develop in.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use shared::{PullSummary, is_branch_name, worktree_branch};

use crate::error::{Error, Result};
use crate::git::Git;
use crate::github::split_repo;
use crate::shell_env::ShellEnv;

const HOST: &str = "github.com";
/// Git lock files older than this were left by a crash, not by a running git.
const STALE_LOCK: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    /// Edits or commits of yours are in the way, so the worktree was not moved.
    /// The PR head is still complete in the clone, so review views work.
    LocalChanges,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    Removed,
    Kept(String),
}

pub struct Workspace {
    root: PathBuf,
    /// `https://github.com` in the app; a `file://` directory in tests.
    remote_base: String,
}

impl Workspace {
    /// `$PR_REVIEWS_HOME`, else `$XDG_DATA_HOME/pr-reviews`, else
    /// `~/.local/share/pr-reviews`.
    pub fn from_env(env: &ShellEnv) -> Result<Self> {
        let root = match (env.var("PR_REVIEWS_HOME"), env.var("XDG_DATA_HOME")) {
            (Some(home), _) => PathBuf::from(home),
            (None, Some(data)) => Path::new(data).join("pr-reviews"),
            (None, None) => env
                .home()
                .ok_or_else(|| Error::msg("HOME is not set"))?
                .join(".local/share/pr-reviews"),
        };
        Ok(Workspace::new(root, "https://github.com"))
    }

    pub fn new(root: PathBuf, remote_base: &str) -> Self {
        Workspace {
            root,
            remote_base: remote_base.trim_end_matches('/').to_string(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repo_dir(&self, repo: &str) -> Result<PathBuf> {
        let (owner, name) = split_repo(repo)?;
        Ok(self.root.join(HOST).join(owner).join(name))
    }

    pub fn worktree_dir(&self, repo: &str, number: u32) -> Result<PathBuf> {
        Ok(self.repo_dir(repo)?.join(format!("pr-{number}")))
    }

    /// Clones the repository unless it already is. Returns its directory.
    pub async fn ensure_repo(&self, git: &Git, repo: &str) -> Result<PathBuf> {
        let dir = self.repo_dir(repo)?;
        let bare = dir.join(".bare");
        if bare.join("HEAD").exists() {
            return Ok(dir);
        }

        // Clone next to the final location and rename, so an interrupted clone
        // never looks finished.
        std::fs::create_dir_all(&dir)?;
        let partial = dir.join(".bare-partial");
        if partial.exists() {
            std::fs::remove_dir_all(&partial)?;
        }
        let url = format!("{}/{repo}.git", self.remote_base);
        git.run(
            &dir,
            &[
                "clone",
                "--quiet",
                "--bare",
                "--filter=blob:none",
                "--no-tags",
                &url,
                ".bare-partial",
            ],
        )
        .await?;
        std::fs::rename(&partial, &bare)?;
        std::fs::write(dir.join(".git"), "gitdir: ./.bare\n")?;

        // A bare clone mirrors every branch into refs/heads. Track them as
        // remote branches instead (git won't update a branch a worktree has
        // checked out), and drop the mirrors so each worktree's branch list
        // shows only the review branches.
        git.run(
            &dir,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )
        .await?;
        let mirrored = git
            .run(
                &dir,
                &["for-each-ref", "--format=delete %(refname)", "refs/heads/"],
            )
            .await?;
        git.run_with_input(&dir, &["update-ref", "--stdin"], &mirrored)
            .await?;
        Ok(dir)
    }

    /// One fetch for all of a repository's pull requests: their heads (kept
    /// under GitHub's own `refs/pull/<n>/head`, which can't clash with a branch
    /// name), their same-repo head branches and their base branches. Returns
    /// each PR head as it was before the fetch, i.e. the head last synced,
    /// which is what makes updating a worktree safe.
    pub async fn fetch(
        &self,
        git: &Git,
        repo: &str,
        prs: &[&PullSummary],
    ) -> Result<HashMap<u32, String>> {
        let dir = self.repo_dir(repo)?;
        let previous: HashMap<u32, String> = git
            .run(
                &dir,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/pull/",
                ],
            )
            .await?
            .lines()
            .filter_map(|line| {
                let (name, sha) = line.split_once(' ')?;
                Some((
                    name.strip_prefix("refs/pull/")?
                        .strip_suffix("/head")?
                        .parse()
                        .ok()?,
                    sha.to_string(),
                ))
            })
            .collect();

        let pulls: Vec<String> = prs
            .iter()
            .map(|pr| format!("+refs/pull/{0}/head:refs/pull/{0}/head", pr.number))
            .collect();
        let branches: BTreeSet<String> = prs
            .iter()
            .flat_map(|pr| {
                let head = (!pr.is_fork).then_some(pr.head_ref.as_str());
                head.into_iter().chain([pr.base_ref.as_str()])
            })
            .filter(|branch| is_branch_name(branch))
            .map(|branch| format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"))
            .collect();

        let mut args = vec!["fetch", "--quiet", "--no-tags", "origin"];
        args.extend(pulls.iter().map(String::as_str));
        let pulls_only = args.len();
        args.extend(branches.iter().map(String::as_str));
        clear_stale_locks(&dir.join(".bare"));
        if git.run(&dir, &args).await.is_err() {
            // A branch can vanish between the inbox query and this fetch (a PR
            // merged with its branch deleted); the pull refs never do.
            args.truncate(pulls_only);
            if git.run(&dir, &args).await.is_err() {
                // Then it's the network or a crashed git's leftovers: tidy up,
                // give it a moment, and try once more.
                clear_stale_locks(&dir.join(".bare"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                git.run(&dir, &args).await?;
            }
        }
        Ok(previous)
    }

    /// Makes every file of commit `sha` local in one batched download. A
    /// blobless clone only holds the contents of what was checked out, and git
    /// would otherwise fetch each missing file on its own (seconds apiece) the
    /// first time the review views read them.
    pub async fn ensure_contents(&self, git: &Git, repo: &str, sha: &str) -> Result<()> {
        let tree = git
            .run(
                &self.repo_dir(repo)?,
                &[
                    "ls-tree",
                    "-r",
                    "-z",
                    "--format=%(objecttype) %(objectname)",
                    sha,
                ],
            )
            .await?;
        self.fetch_missing(git, repo, &tree).await
    }

    /// Like [`Workspace::ensure_contents`], for just `paths` in commit `sha`,
    /// e.g. the old versions of the files a pull request changes. Paths that
    /// aren't in the commit are skipped.
    pub async fn ensure_files(
        &self,
        git: &Git,
        repo: &str,
        sha: &str,
        paths: &[String],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "ls-tree",
            "-z",
            "--format=%(objecttype) %(objectname)",
            sha,
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let tree = git.run(&self.repo_dir(repo)?, &args).await?;
        self.fetch_missing(git, repo, &tree).await
    }

    /// Downloads, in one batch, the blobs of an `ls-tree` listing that the
    /// clone doesn't have yet.
    async fn fetch_missing(&self, git: &Git, repo: &str, listing: &str) -> Result<()> {
        let dir = self.repo_dir(repo)?;
        let blobs: String = listing
            .split('\0')
            .filter_map(|entry| entry.strip_prefix("blob "))
            .map(|oid| format!("{oid}\n"))
            .collect();
        if blobs.is_empty() {
            return Ok(());
        }
        let checked = git
            .offline()
            .run_with_input(
                &dir,
                &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
                &blobs,
            )
            .await?;
        let missing: String = checked
            .lines()
            .filter_map(|line| line.strip_suffix(" missing"))
            .map(|oid| format!("{oid}\n"))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        // What git itself runs to fill in a partial clone, for all at once.
        git.run_with_input(
            &dir,
            &[
                "-c",
                "fetch.negotiationAlgorithm=noop",
                "fetch",
                "origin",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--recurse-submodules=no",
                "--filter=blob:none",
                "--stdin",
            ],
            &missing,
        )
        .await?;
        Ok(())
    }

    /// Brings a pull request's worktree to the freshly fetched
    /// `refs/pull/<n>/head`, and returns that head. `previous` is the ref
    /// before the fetch.
    ///
    /// This is built to keep working: force-pushes simply replace the
    /// worktree's commit, a worktree git can no longer update is rebuilt, and
    /// even when your changes hold the worktree back, the new head's files are
    /// made local so the review views still show the PR as it is now.
    pub async fn sync_worktree(
        &self,
        git: &Git,
        pr: &PullSummary,
        previous: Option<&str>,
    ) -> Result<(Outcome, String)> {
        let dir = self.repo_dir(&pr.repo)?;
        let target = git
            .run(
                &dir,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/pull/{}/head^{{commit}}", pr.number),
                ],
            )
            .await?
            .trim()
            .to_string();
        // Best effort: without it the contents still arrive, just lazily.
        let _ = self.ensure_contents(git, &pr.repo, &target).await;

        let outcome = match self.place_worktree(git, pr, &target, previous).await {
            Ok(outcome) => outcome,
            Err(error) => self
                .rebuild_worktree(git, pr, &target)
                .await
                .map_err(|again| Error::msg(format!("{error} (and rebuilding failed: {again})")))?,
        };
        Ok((outcome, target))
    }

    async fn place_worktree(
        &self,
        git: &Git,
        pr: &PullSummary,
        target: &str,
        previous: Option<&str>,
    ) -> Result<Outcome> {
        let dir = self.repo_dir(&pr.repo)?;
        let worktree = self.worktree_dir(&pr.repo, pr.number)?;
        if !worktree.exists() {
            create_worktree(git, &dir, &worktree, pr, target).await?;
            return Ok(Outcome::Ready);
        }

        let head = git.run(&worktree, &["rev-parse", "HEAD"]).await?;
        let head = head.trim();
        let branch = review_branch(git, &dir, &worktree, pr).await?;
        let current = current_branch(git, &worktree).await;
        if head == target && current.as_deref() == Some(branch.as_str()) {
            return Ok(Outcome::Ready);
        }
        if is_dirty(git, &worktree).await? {
            return Ok(Outcome::LocalChanges);
        }
        // Moving is safe when HEAD is exactly what we synced last time (so a
        // force-push may replace it) or when the new head contains it.
        let ours = previous == Some(head)
            || git
                .check(&worktree, &["merge-base", "--is-ancestor", head, target])
                .await?;
        if !ours {
            return Ok(Outcome::LocalChanges);
        }
        move_worktree(git, &worktree, pr, &branch, target, current).await?;
        Ok(Outcome::Ready)
    }

    /// Starts a worktree over after git failed to update it (a stale lock, a
    /// deleted admin directory, a half-finished checkout). The worktree is the
    /// app's, so that's fine, unless it holds your work: then it stays put.
    async fn rebuild_worktree(&self, git: &Git, pr: &PullSummary, target: &str) -> Result<Outcome> {
        let dir = self.repo_dir(&pr.repo)?;
        let worktree = self.worktree_dir(&pr.repo, pr.number)?;
        if worktree.exists() {
            match is_dirty(git, &worktree).await {
                Ok(true) => return Ok(Outcome::LocalChanges),
                Ok(false) => {
                    let path = worktree.to_string_lossy();
                    let _ = git
                        .run(&dir, &["worktree", "remove", "--force", "--force", &path])
                        .await;
                    if worktree.exists() {
                        std::fs::remove_dir_all(&worktree)?;
                    }
                }
                Err(_) => {
                    // Too broken for git to say whether anything in it is
                    // yours, so move it aside rather than delete it.
                    let stamp = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let aside = dir
                        .join(".broken")
                        .join(format!("pr-{}-{stamp}", pr.number));
                    std::fs::create_dir_all(dir.join(".broken"))?;
                    std::fs::rename(&worktree, &aside)?;
                }
            }
        }
        clear_stale_locks(&dir.join(".bare"));
        create_worktree(git, &dir, &worktree, pr, target).await?;
        Ok(Outcome::Ready)
    }

    /// *Update anyway*: moves a held-back worktree to the PR head, first
    /// saving your work: uncommitted changes to the stash, commits that are
    /// nowhere else to a `backup/…` branch. Returns where things went.
    pub async fn force_update(&self, git: &Git, pr: &PullSummary) -> Result<String> {
        let dir = self.repo_dir(&pr.repo)?;
        let worktree = self.worktree_dir(&pr.repo, pr.number)?;
        let target = git
            .run(
                &dir,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/pull/{}/head^{{commit}}", pr.number),
                ],
            )
            .await?
            .trim()
            .to_string();
        if !worktree.exists() {
            create_worktree(git, &dir, &worktree, pr, &target).await?;
            return Ok("Created the worktree.".into());
        }

        let short = &target[..target.len().min(12)];
        let mut saved = Vec::new();
        if is_dirty(git, &worktree).await? {
            let message = format!("squint: pr-{} before updating to {short}", pr.number);
            git.run(
                &worktree,
                &[
                    // Stashing makes a commit, and the app's git runs without
                    // your global config, so it needs a name of its own.
                    "-c",
                    "user.name=Squint",
                    "-c",
                    "user.email=squint@localhost",
                    "stash",
                    "push",
                    "--quiet",
                    "--include-untracked",
                    "--message",
                    &message,
                ],
            )
            .await?;
            saved.push(format!(
                "your uncommitted changes are stashed as \"{message}\""
            ));
        }
        let unpushed = git
            .run(
                &worktree,
                &[
                    "rev-list",
                    "-n1",
                    "HEAD",
                    "--not",
                    "--remotes",
                    "--glob=refs/pull/*/head",
                ],
            )
            .await?;
        if !unpushed.trim().is_empty() {
            let head = git
                .run(&worktree, &["rev-parse", "--short=12", "HEAD"])
                .await?;
            let backup = format!("backup/pr-{}-{}", pr.number, head.trim());
            git.run(
                &worktree,
                &["branch", "--quiet", "--force", &backup, "HEAD"],
            )
            .await?;
            saved.push(format!("your commits are on branch {backup}"));
        }

        let branch = review_branch(git, &dir, &worktree, pr).await?;
        let current = current_branch(git, &worktree).await;
        move_worktree(git, &worktree, pr, &branch, &target, current).await?;
        Ok(if saved.is_empty() {
            format!("Updated pr-{} to {short}.", pr.number)
        } else {
            format!(
                "Updated pr-{} to {short}; {}.",
                pr.number,
                saved.join(", and ")
            )
        })
    }

    /// Every pull request worktree on disk, as `(repo, number)`.
    pub fn worktrees(&self) -> Vec<(String, u32)> {
        let mut found = Vec::new();
        for (repo, dir) in self.repos() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = entry.file_name();
                if let Some(number) = name
                    .to_str()
                    .and_then(|n| n.strip_prefix("pr-"))
                    .and_then(|n| n.parse().ok())
                {
                    found.push((repo.clone(), number));
                }
            }
        }
        found
    }

    /// Every repository directory on disk, as `(owner/name, path)`.
    pub fn repos(&self) -> Vec<(String, PathBuf)> {
        let dirs = |path: &Path| -> Vec<(String, PathBuf)> {
            std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
                .filter(|(name, _)| !name.starts_with('.'))
                .collect()
        };
        dirs(&self.root.join(HOST))
            .into_iter()
            .flat_map(|(owner, path)| {
                dirs(&path)
                    .into_iter()
                    .map(move |(name, path)| (format!("{owner}/{name}"), path))
            })
            // Only directories the app cloned; anything else is left alone.
            .filter(|(_, path)| path.join(".bare").exists() || path.join(".bare-partial").exists())
            .collect()
    }

    /// Removes a gone pull request's worktree, unless it holds work of yours.
    pub async fn remove_if_unused(&self, git: &Git, repo: &str, number: u32) -> Result<Removal> {
        let worktree = self.worktree_dir(repo, number)?;
        if !worktree.join(".git").exists() {
            self.forget(git, repo, number, None).await?;
            return Ok(Removal::Removed);
        }
        if is_dirty(git, &worktree).await? {
            return Ok(Removal::Kept("uncommitted changes".into()));
        }
        let unpushed = git
            .run(
                &worktree,
                &[
                    "rev-list",
                    "-n1",
                    "HEAD",
                    "--not",
                    "--remotes",
                    "--glob=refs/pull/*/head",
                ],
            )
            .await?;
        if !unpushed.trim().is_empty() {
            return Ok(Removal::Kept("commits that are not on GitHub".into()));
        }
        self.remove(git, repo, number).await?;
        Ok(Removal::Removed)
    }

    /// Removes a pull request's worktree whatever it holds.
    pub async fn remove(&self, git: &Git, repo: &str, number: u32) -> Result<()> {
        let dir = self.repo_dir(repo)?;
        let worktree = self.worktree_dir(repo, number)?;
        let branch = current_branch(git, &worktree).await;
        if worktree.exists() {
            let path = worktree.to_string_lossy();
            git.run(&dir, &["worktree", "remove", "--force", "--force", &path])
                .await?;
        }
        self.forget(git, repo, number, branch).await
    }

    async fn forget(
        &self,
        git: &Git,
        repo: &str,
        number: u32,
        branch: Option<String>,
    ) -> Result<()> {
        let dir = self.repo_dir(repo)?;
        let _ = git.run(&dir, &["worktree", "prune"]).await;
        if let Some(branch) = branch {
            let _ = git.run(&dir, &["branch", "--quiet", "-D", &branch]).await;
        }
        let _ = git
            .run(
                &dir,
                &["update-ref", "-d", &format!("refs/pull/{number}/head")],
            )
            .await;
        Ok(())
    }

    /// Deletes a repository's directory once it has no worktrees left.
    pub async fn remove_repo_if_empty(&self, repo: &str) -> Result<bool> {
        let dir = self.repo_dir(repo)?;
        if self.worktrees().iter().any(|(r, _)| r == repo) || !dir.exists() {
            return Ok(false);
        }
        let owner = dir.parent().map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || {
            std::fs::remove_dir_all(&dir)?;
            // Tidy the owner directory too when this was its last repository.
            if let Some(owner) = owner {
                let _ = std::fs::remove_dir(owner);
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|e| Error::msg(e.to_string()))??;
        Ok(true)
    }
}

async fn create_worktree(
    git: &Git,
    dir: &Path,
    worktree: &Path,
    pr: &PullSummary,
    target: &str,
) -> Result<()> {
    // Forget worktrees whose directory was deleted by hand.
    git.run(dir, &["worktree", "prune"]).await?;
    let branch = review_branch(git, dir, worktree, pr).await?;
    let path = worktree.to_string_lossy();
    git.run(
        dir,
        &["worktree", "add", "--quiet", "-B", &branch, &path, target],
    )
    .await?;
    set_upstream(git, worktree, pr, &branch).await;
    Ok(())
}

async fn move_worktree(
    git: &Git,
    worktree: &Path,
    pr: &PullSummary,
    branch: &str,
    target: &str,
    current: Option<String>,
) -> Result<()> {
    git.run(worktree, &["checkout", "--quiet", "-B", branch, target])
        .await?;
    if let Some(old) = current.filter(|old| old != branch) {
        // The head branch was renamed; the old local branch is ours to drop.
        let _ = git.run(worktree, &["branch", "--quiet", "-D", &old]).await;
    }
    set_upstream(git, worktree, pr, branch).await;
    Ok(())
}

/// The branch this PR's worktree should be on, given the other worktrees.
async fn review_branch(git: &Git, dir: &Path, worktree: &Path, pr: &PullSummary) -> Result<String> {
    let taken = other_worktree_branches(git, dir, worktree).await?;
    Ok(worktree_branch(pr, |b| taken.contains(b)))
}

/// Deletes `*.lock` files a crashed git left in the clone. Fresh ones belong
/// to a git that is still running and are left alone.
fn clear_stale_locks(bare: &Path) {
    let mut dirs = vec![bare.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // Loose objects live in 256 directories that never hold locks.
                let name = entry.file_name();
                let object_dir = dir.ends_with("objects")
                    && name.len() == 2
                    && name
                        .to_string_lossy()
                        .chars()
                        .all(|c| c.is_ascii_hexdigit());
                if !object_dir {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "lock")
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().unwrap_or_default() > STALE_LOCK)
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

async fn current_branch(git: &Git, worktree: &Path) -> Option<String> {
    if !worktree.exists() {
        return None;
    }
    let branch = git
        .run(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()?;
    Some(branch.trim().to_string()).filter(|b| !b.is_empty())
}

async fn is_dirty(git: &Git, worktree: &Path) -> Result<bool> {
    let status = git.run(worktree, &["status", "--porcelain"]).await?;
    Ok(!status.trim().is_empty())
}

/// Branches checked out by the repository's other worktrees.
async fn other_worktree_branches(
    git: &Git,
    dir: &Path,
    worktree: &Path,
) -> Result<HashSet<String>> {
    let list = git.run(dir, &["worktree", "list", "--porcelain"]).await?;
    let this = worktree.to_string_lossy();
    let mut branches = HashSet::new();
    let mut path = "";
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = p;
        } else if let Some(b) = line.strip_prefix("branch refs/heads/")
            && !same_path(path, &this)
        {
            branches.insert(b.to_string());
        }
    }
    Ok(branches)
}

/// Compares worktree paths as git prints them (it resolves symlinks such as
/// macOS's `/var` → `/private/var`).
fn same_path(a: &str, b: &str) -> bool {
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    a == b || canonical(a) == canonical(b)
}

/// Same-repo PRs track their remote branch, so Zed shows ahead/behind and a
/// fix-up can be pushed. Best effort: a missing remote branch just means no
/// upstream.
async fn set_upstream(git: &Git, worktree: &Path, pr: &PullSummary, branch: &str) {
    if !pr.is_fork && branch == pr.head_ref {
        let upstream = format!("--set-upstream-to=origin/{branch}");
        let _ = git.run(worktree, &["branch", "--quiet", &upstream]).await;
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::process::Command;

    /// A local stand-in for GitHub: `<dir>/remotes/o/r.git` with a `main`
    /// branch and pull requests published under `refs/pull/<n>/head`.
    pub struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        pub fn new() -> Self {
            let fixture = Fixture {
                dir: tempfile::tempdir().unwrap(),
            };
            let remote = fixture.remote();
            std::fs::create_dir_all(&remote).unwrap();
            fixture.sh(&remote, &["init", "--quiet", "--bare", "-b", "main"]);
            fixture.sh(&remote, &["config", "uploadpack.allowFilter", "true"]);
            // GitHub serves objects by id, which is how partial clones fill in.
            fixture.sh(
                &remote,
                &["config", "uploadpack.allowAnySHA1InWant", "true"],
            );
            let src = fixture.src();
            std::fs::create_dir_all(&src).unwrap();
            fixture.sh(&src, &["init", "--quiet", "-b", "main"]);
            fixture.commit("README.md", "hello\n");
            fixture.sh(&src, &["remote", "add", "origin", remote.to_str().unwrap()]);
            fixture.sh(&src, &["push", "--quiet", "origin", "main"]);
            fixture
        }

        pub fn remote(&self) -> PathBuf {
            self.dir.path().join("remotes/o/r.git")
        }

        pub fn src(&self) -> PathBuf {
            self.dir.path().join("src")
        }

        pub fn workspace(&self) -> Workspace {
            let base = format!("file://{}", self.dir.path().join("remotes").display());
            Workspace::new(self.dir.path().join("ws"), &base)
        }

        pub fn git(&self) -> Git {
            Git::with_program("git".into(), std::env::var_os("PATH").unwrap(), None)
        }

        pub fn sh(&self, dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        pub fn commit(&self, file: &str, content: &str) -> String {
            std::fs::write(self.src().join(file), content).unwrap();
            self.sh(&self.src(), &["add", "."]);
            self.sh(&self.src(), &["commit", "--quiet", "-m", file]);
            self.sh(&self.src(), &["rev-parse", "HEAD"])
        }

        /// Publishes the source repo's HEAD as branch `branch` and PR `number`.
        pub fn publish(&self, branch: &str, number: u32) {
            let src = self.src();
            self.sh(
                &src,
                &[
                    "push",
                    "--quiet",
                    "--force",
                    "origin",
                    &format!("HEAD:refs/heads/{branch}"),
                ],
            );
            self.sh(
                &src,
                &[
                    "push",
                    "--quiet",
                    "--force",
                    "origin",
                    &format!("HEAD:refs/pull/{number}/head"),
                ],
            );
        }

        pub fn pr(&self, number: u32, branch: &str, is_fork: bool) -> PullSummary {
            PullSummary {
                repo: "o/r".into(),
                number,
                title: String::new(),
                url: String::new(),
                author: None,
                is_draft: false,
                updated_at: String::new(),
                head_ref: branch.into(),
                base_ref: "main".into(),
                head_sha: String::new(),
                is_fork,
                additions: 0,
                deletions: 0,
                changed_files: 0,
                review_decision: None,
                checks: None,
                roles: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Fixture;
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    async fn sync(ws: &Workspace, git: &Git, pr: &PullSummary) -> Outcome {
        ws.ensure_repo(git, &pr.repo).await.unwrap();
        let previous = ws.fetch(git, &pr.repo, &[pr]).await.unwrap();
        ws.sync_worktree(git, pr, previous.get(&pr.number).map(String::as_str))
            .await
            .unwrap()
            .0
    }

    #[test]
    fn clones_and_checks_out_the_pr_branch() {
        let fx = Fixture::new();
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "feat/x"]);
        let head = fx.commit("a.txt", "a\n");
        fx.publish("feat/x", 1);
        let (ws, git) = (fx.workspace(), fx.git());
        let pr = fx.pr(1, "feat/x", false);

        block_on(async {
            assert_eq!(sync(&ws, &git, &pr).await, Outcome::Ready);
        });
        let wt = ws.worktree_dir("o/r", 1).unwrap();
        assert_eq!(fx.sh(&wt, &["rev-parse", "HEAD"]), head);
        assert_eq!(fx.sh(&wt, &["symbolic-ref", "--short", "HEAD"]), "feat/x");
        assert_eq!(
            fx.sh(&wt, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
            "origin/feat/x"
        );
        // Only the review branch is local; the clone's mirrored branches are gone.
        assert_eq!(
            fx.sh(&wt, &["branch", "--format=%(refname:short)"]),
            "feat/x"
        );
        assert!(ws.repo_dir("o/r").unwrap().join(".git").is_file());
    }

    #[test]
    fn forks_get_a_pr_branch() {
        let fx = Fixture::new();
        fx.commit("a.txt", "a\n");
        fx.publish("patch-1", 2);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &fx.pr(2, "patch-1", true)));
        let wt = ws.worktree_dir("o/r", 2).unwrap();
        assert_eq!(fx.sh(&wt, &["symbolic-ref", "--short", "HEAD"]), "pr/2");
    }

    #[test]
    fn follows_new_commits_and_force_pushes() {
        let fx = Fixture::new();
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "feat/x"]);
        fx.commit("a.txt", "1\n");
        fx.publish("feat/x", 1);
        let (ws, git) = (fx.workspace(), fx.git());
        let pr = fx.pr(1, "feat/x", false);
        let wt = ws.worktree_dir("o/r", 1).unwrap();
        block_on(sync(&ws, &git, &pr));

        let second = fx.commit("a.txt", "2\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::Ready);
        assert_eq!(fx.sh(&wt, &["rev-parse", "HEAD"]), second);

        fx.sh(&fx.src(), &["reset", "--quiet", "--hard", "HEAD~1"]);
        let rewritten = fx.commit("b.txt", "rewritten\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::Ready);
        assert_eq!(fx.sh(&wt, &["rev-parse", "HEAD"]), rewritten);
    }

    #[test]
    fn never_moves_a_worktree_with_your_work() {
        let fx = Fixture::new();
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "feat/x"]);
        fx.commit("a.txt", "1\n");
        fx.publish("feat/x", 1);
        let (ws, git) = (fx.workspace(), fx.git());
        let pr = fx.pr(1, "feat/x", false);
        let wt = ws.worktree_dir("o/r", 1).unwrap();
        block_on(sync(&ws, &git, &pr));

        // An uncommitted edit.
        std::fs::write(wt.join("a.txt"), "mine\n").unwrap();
        fx.commit("a.txt", "2\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::LocalChanges);
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "mine\n");

        // A local commit, while the PR is force-pushed underneath it.
        fx.sh(&wt, &["commit", "--quiet", "-am", "mine"]);
        let mine = fx.sh(&wt, &["rev-parse", "HEAD"]);
        fx.sh(&fx.src(), &["reset", "--quiet", "--hard", "HEAD~1"]);
        fx.commit("c.txt", "3\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::LocalChanges);
        assert_eq!(fx.sh(&wt, &["rev-parse", "HEAD"]), mine);
    }

    #[test]
    fn renames_the_branch_with_the_pr() {
        let fx = Fixture::new();
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "old-name"]);
        fx.commit("a.txt", "1\n");
        fx.publish("old-name", 1);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &fx.pr(1, "old-name", false)));

        fx.publish("new-name", 1);
        assert_eq!(
            block_on(sync(&ws, &git, &fx.pr(1, "new-name", false))),
            Outcome::Ready
        );
        let wt = ws.worktree_dir("o/r", 1).unwrap();
        assert_eq!(fx.sh(&wt, &["symbolic-ref", "--short", "HEAD"]), "new-name");
        assert_eq!(
            fx.sh(&wt, &["branch", "--format=%(refname:short)"]),
            "new-name"
        );
    }

    #[test]
    fn two_prs_from_one_branch_both_get_worktrees() {
        let fx = Fixture::new();
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "shared"]);
        fx.commit("a.txt", "1\n");
        fx.publish("shared", 1);
        fx.publish("shared", 2);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &fx.pr(1, "shared", false)));
        block_on(sync(&ws, &git, &fx.pr(2, "shared", false)));
        let branch = |n| {
            fx.sh(
                &ws.worktree_dir("o/r", n).unwrap(),
                &["symbolic-ref", "--short", "HEAD"],
            )
        };
        assert_eq!(branch(1), "shared");
        assert_eq!(branch(2), "pr/2");
    }

    #[test]
    fn cleans_up_gone_prs_but_keeps_your_work() {
        let fx = Fixture::new();
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "feat/x"]);
        fx.commit("a.txt", "1\n");
        fx.publish("feat/x", 1);
        fx.publish("feat/x", 2);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &fx.pr(1, "feat/x", false)));
        block_on(sync(&ws, &git, &fx.pr(2, "feat/x", false)));
        assert_eq!(ws.worktrees().len(), 2);

        let kept = ws.worktree_dir("o/r", 2).unwrap();
        std::fs::write(kept.join("notes.md"), "todo\n").unwrap();
        block_on(async {
            assert_eq!(
                ws.remove_if_unused(&git, "o/r", 1).await.unwrap(),
                Removal::Removed
            );
            assert_eq!(
                ws.remove_if_unused(&git, "o/r", 2).await.unwrap(),
                Removal::Kept("uncommitted changes".into())
            );
            assert!(!ws.remove_repo_if_empty("o/r").await.unwrap());

            ws.remove(&git, "o/r", 2).await.unwrap();
            assert!(ws.remove_repo_if_empty("o/r").await.unwrap());
        });
        assert!(!ws.worktree_dir("o/r", 1).unwrap().exists());
        assert!(!ws.repo_dir("o/r").unwrap().exists());
    }

    #[test]
    fn keeps_unpushed_commits() {
        let fx = Fixture::new();
        fx.commit("a.txt", "1\n");
        fx.publish("feat/x", 1);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &fx.pr(1, "feat/x", false)));
        let wt = ws.worktree_dir("o/r", 1).unwrap();
        std::fs::write(wt.join("a.txt"), "mine\n").unwrap();
        fx.sh(&wt, &["commit", "--quiet", "-am", "mine"]);
        assert_eq!(
            block_on(ws.remove_if_unused(&git, "o/r", 1)).unwrap(),
            Removal::Kept("commits that are not on GitHub".into())
        );
    }

    /// Ages a file so it looks like a crashed git left it behind.
    fn make_stale(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(60 * 60);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    fn pr_branch(fx: &Fixture) -> PullSummary {
        fx.sh(&fx.src(), &["checkout", "--quiet", "-b", "feat/x"]);
        fx.commit("a.txt", "1\n");
        fx.publish("feat/x", 1);
        fx.pr(1, "feat/x", false)
    }

    #[test]
    fn held_back_worktrees_still_have_the_new_head_locally() {
        let fx = Fixture::new();
        let pr = pr_branch(&fx);
        let (ws, git) = (fx.workspace(), fx.git());
        let (dir, wt) = (
            ws.repo_dir("o/r").unwrap(),
            ws.worktree_dir("o/r", 1).unwrap(),
        );
        block_on(sync(&ws, &git, &pr));

        // Main moves on with a file no worktree ever checks out: proof that the
        // clone really is partial, so the next assertion means something.
        fx.sh(&fx.src(), &["checkout", "--quiet", "main"]);
        fx.commit("main-only.txt", "base\n");
        fx.sh(&fx.src(), &["push", "--quiet", "origin", "main"]);

        // You edit the worktree; the author force-pushes a new file.
        std::fs::write(wt.join("a.txt"), "mine\n").unwrap();
        fx.sh(&fx.src(), &["checkout", "--quiet", "feat/x"]);
        fx.sh(&fx.src(), &["reset", "--quiet", "--hard", "HEAD~1"]);
        let new_head = fx.commit("new.txt", "fresh\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::LocalChanges);

        block_on(async {
            let offline = git.offline();
            let read = |rev: String| {
                let (offline, dir) = (offline.clone(), dir.clone());
                async move { offline.run(&dir, &["cat-file", "-p", &rev]).await }
            };
            assert!(
                read("refs/remotes/origin/main:main-only.txt".into())
                    .await
                    .is_err()
            );
            assert_eq!(
                read(format!("{new_head}:new.txt")).await.unwrap(),
                "fresh\n"
            );
        });
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "mine\n");
    }

    #[test]
    fn a_crashed_gits_lock_does_not_block_syncing() {
        let fx = Fixture::new();
        let pr = pr_branch(&fx);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &pr));

        let lock = ws
            .repo_dir("o/r")
            .unwrap()
            .join(".bare/refs/pull/1/head.lock");
        std::fs::write(&lock, "").unwrap();
        make_stale(&lock);
        let head = fx.commit("a.txt", "2\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::Ready);
        assert_eq!(
            fx.sh(&ws.worktree_dir("o/r", 1).unwrap(), &["rev-parse", "HEAD"]),
            head
        );
        assert!(!lock.exists());
    }

    #[test]
    fn a_fresh_lock_belongs_to_a_running_git_and_is_left_alone() {
        let fx = Fixture::new();
        let pr = pr_branch(&fx);
        let (ws, git) = (fx.workspace(), fx.git());
        block_on(sync(&ws, &git, &pr));
        let lock = ws
            .repo_dir("o/r")
            .unwrap()
            .join(".bare/refs/pull/1/head.lock");
        std::fs::write(&lock, "").unwrap();
        fx.commit("a.txt", "2\n");
        fx.publish("feat/x", 1);
        block_on(async {
            ws.ensure_repo(&git, "o/r").await.unwrap();
            assert!(ws.fetch(&git, "o/r", &[&pr]).await.is_err());
        });
        assert!(lock.exists());
    }

    #[test]
    fn rebuilds_a_worktree_git_can_no_longer_use() {
        let fx = Fixture::new();
        let pr = pr_branch(&fx);
        let (ws, git) = (fx.workspace(), fx.git());
        let dir = ws.repo_dir("o/r").unwrap();
        block_on(sync(&ws, &git, &pr));

        // Wipe the worktree's admin directory: its `.git` now points nowhere.
        std::fs::remove_dir_all(dir.join(".bare/worktrees/pr-1")).unwrap();
        let head = fx.commit("a.txt", "2\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::Ready);

        let wt = ws.worktree_dir("o/r", 1).unwrap();
        assert_eq!(fx.sh(&wt, &["rev-parse", "HEAD"]), head);
        assert_eq!(fx.sh(&wt, &["symbolic-ref", "--short", "HEAD"]), "feat/x");
        // Git couldn't vouch for its contents, so the old copy was moved aside.
        assert_eq!(std::fs::read_dir(dir.join(".broken")).unwrap().count(), 1);
    }

    #[test]
    fn update_anyway_saves_your_work_first() {
        let fx = Fixture::new();
        let pr = pr_branch(&fx);
        let (ws, git) = (fx.workspace(), fx.git());
        let wt = ws.worktree_dir("o/r", 1).unwrap();
        block_on(sync(&ws, &git, &pr));

        std::fs::write(wt.join("a.txt"), "my commit\n").unwrap();
        fx.sh(&wt, &["commit", "--quiet", "-am", "mine"]);
        let mine = fx.sh(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("notes.md"), "draft\n").unwrap();
        fx.sh(&fx.src(), &["reset", "--quiet", "--hard", "HEAD~1"]);
        let head = fx.commit("b.txt", "force-pushed\n");
        fx.publish("feat/x", 1);
        assert_eq!(block_on(sync(&ws, &git, &pr)), Outcome::LocalChanges);

        let message = block_on(ws.force_update(&git, &pr)).unwrap();
        assert!(message.contains("stashed"), "{message}");
        assert!(message.contains("backup/pr-1-"), "{message}");
        assert_eq!(fx.sh(&wt, &["rev-parse", "HEAD"]), head);
        assert!(!wt.join("notes.md").exists());
        assert!(
            fx.sh(&wt, &["stash", "list"])
                .contains("squint: pr-1 before updating")
        );
        let backup = fx.sh(
            &wt,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads/backup/",
            ],
        );
        assert_eq!(backup, mine);
    }
}
