//! Types exchanged between the Tauri backend and the Leptos UI over IPC, plus the
//! diff model both sides agree on. Living in one crate means a field rename is a
//! compile error on both ends instead of a silent `undefined` at runtime.

pub mod actions;
pub mod code;
pub mod diff;

use serde::{Deserialize, Serialize};

pub use actions::{
    CheckRun, CheckSuite, Checks, Job, JobStep, RunState, StatusCheck, StepLog, WorkflowRun,
};
pub use code::{
    CodeTree, DiffShas, FileBody, FileContent, GrepMatch, GrepQuery, GrepResult, LineHighlights,
    TreeNode,
};
pub use diff::{DiffLine, FileDiff, FileStatus, HeadChanges, Hunk, LineKind, SplitRow};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub login: String,
    pub avatar_url: String,
}

/// Why a pull request shows up in the inbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Author,
    ReviewRequested,
    Assignee,
    /// Mentioned or commented, without a more specific role.
    Involved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Success,
    Failure,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PullSummary {
    /// `owner/name`.
    pub repo: String,
    pub number: u32,
    pub title: String,
    pub url: String,
    pub author: Option<Actor>,
    pub is_draft: bool,
    /// RFC 3339 timestamp of the latest activity: GitHub's `updatedAt`, or the
    /// newest comment or review when that is later.
    pub updated_at: String,
    pub head_ref: String,
    pub base_ref: String,
    /// Older cached inboxes lack the next two fields; they fill in on refresh.
    #[serde(default)]
    pub head_sha: String,
    /// The head branch lives in another repository.
    #[serde(default)]
    pub is_fork: bool,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub review_decision: Option<ReviewDecision>,
    pub checks: Option<CheckState>,
    pub roles: Vec<Role>,
}

impl PullSummary {
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub viewer: String,
    pub pulls: Vec<PullSummary>,
    /// False when GitHub returned partial results (errors, or more pages than
    /// fetched). Clean-up only trusts complete inboxes.
    #[serde(default)]
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The base version of the file (removed and context lines).
    Old,
    /// The head version of the file (added and context lines).
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub author: Option<Actor>,
    /// HTML rendered and sanitised by GitHub.
    pub body_html: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub author: Option<Actor>,
    pub state: ReviewState,
    pub body_html: String,
    pub submitted_at: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewThread {
    /// GraphQL node id, which resolving takes.
    pub id: String,
    pub path: String,
    /// Line in the current diff; `None` once the thread is outdated, and for
    /// a comment on the whole file.
    pub line: Option<u32>,
    /// The first line of a comment on a range of lines.
    pub start_line: Option<u32>,
    pub side: Side,
    pub is_resolved: bool,
    pub is_outdated: bool,
    /// The REST id of the thread's first comment, which replies answer.
    pub root_comment_id: Option<u64>,
    pub can_reply: bool,
    pub can_resolve: bool,
    pub can_unresolve: bool,
    pub comments: Vec<Comment>,
}

/// A line of one side of a file's diff, where review comments attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Anchor {
    pub side: Side,
    pub line: u32,
}

/// A review comment to post on a line, or a range of lines, of a file's diff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewComment {
    /// The head commit the diff showed; the lines are that commit's.
    pub commit: String,
    pub path: String,
    /// The range's first line, the same as `end` for a single line. The two
    /// may be on different sides: from a removed line to an added one.
    pub start: Anchor,
    pub end: Anchor,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TimelineItem {
    Comment(Comment),
    Review(Review),
    Thread(ReviewThread),
}

impl TimelineItem {
    pub fn timestamp(&self) -> &str {
        match self {
            TimelineItem::Comment(c) => &c.created_at,
            TimelineItem::Review(r) => &r.submitted_at,
            TimelineItem::Thread(t) => t.comments.first().map_or("", |c| c.created_at.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PullDetail {
    pub description: Comment,
    pub head_sha: String,
    /// Conversation comments, reviews and inline threads, oldest first.
    pub timeline: Vec<TimelineItem>,
}

/// Where the GitHub token in use came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    Keychain,
    /// `GH_TOKEN` or `GITHUB_TOKEN`, which take precedence over the Keychain.
    Environment,
}

/// Result of handing a pull request's worktree to Zed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZedLaunch {
    pub worktree: String,
    /// Set when the worktree had local edits and was left on its old commit.
    pub warning: Option<String>,
}

/// Where a pull request's worktree in the review workspace stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncStatus {
    Queued,
    Syncing {
        step: String,
    },
    Ready,
    /// The worktree holds edits or commits of yours, so it was not updated.
    /// The PR's latest head is still complete in the clone.
    LocalChanges,
    /// Refreshing failed (offline, say); the last synced worktree still works
    /// and the next round tries again.
    Stale {
        message: String,
    },
    /// There is no usable worktree yet.
    Failed {
        message: String,
    },
}

/// Emitted as `workspace-sync` whenever a pull request's status changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncEvent {
    pub repo: String,
    pub number: u32,
    pub status: SyncStatus,
}

/// Emitted as `workspace-round` when a sync round starts or finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundEvent {
    pub running: bool,
}

/// A worktree of a pull request that is gone, kept because it holds your work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeptWorktree {
    pub repo: String,
    pub number: u32,
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub root: String,
    pub round_running: bool,
    pub statuses: Vec<SyncEvent>,
    pub kept: Vec<KeptWorktree>,
}

/// The local branch a pull request's worktree is checked out on: the PR's own
/// head branch when it lives in the same repository and no other worktree has
/// it, otherwise `pr/<number>`.
pub fn worktree_branch(pr: &PullSummary, taken: impl Fn(&str) -> bool) -> String {
    if !pr.is_fork && is_branch_name(&pr.head_ref) && !taken(&pr.head_ref) {
        pr.head_ref.clone()
    } else {
        format!("pr/{}", pr.number)
    }
}

/// A conservative subset of git's ref-name rules; anything else falls back to
/// `pr/<number>` rather than reaching git as an argument.
pub fn is_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | '#' | '=' | ',')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(head_ref: &str, is_fork: bool) -> PullSummary {
        PullSummary {
            repo: "o/r".into(),
            number: 7,
            title: String::new(),
            url: String::new(),
            author: None,
            is_draft: false,
            updated_at: String::new(),
            head_ref: head_ref.into(),
            base_ref: "main".into(),
            head_sha: "a".repeat(40),
            is_fork,
            additions: 0,
            deletions: 0,
            changed_files: 0,
            review_decision: None,
            checks: None,
            roles: Vec::new(),
        }
    }

    #[test]
    fn worktrees_use_the_pr_branch_when_possible() {
        let nothing_taken = |_: &str| false;
        assert_eq!(
            worktree_branch(&pr("feat/x", false), nothing_taken),
            "feat/x"
        );
        assert_eq!(worktree_branch(&pr("feat/x", true), nothing_taken), "pr/7");
        assert_eq!(
            worktree_branch(&pr("feat/x", false), |b| b == "feat/x"),
            "pr/7"
        );
        assert_eq!(worktree_branch(&pr("-rf", false), nothing_taken), "pr/7");
    }

    #[test]
    fn rejects_unsafe_branch_names() {
        for good in ["main", "feat/guest-checkout", "renovate/ruby-3.x", "a+b#c"] {
            assert!(is_branch_name(good), "{good}");
        }
        for bad in [
            "", "-x", "a..b", "a b", "a:b", "x.lock", "a//b", "a@{1}", "/a", "a/", "a~1",
        ] {
            assert!(!is_branch_name(bad), "{bad}");
        }
    }
}
