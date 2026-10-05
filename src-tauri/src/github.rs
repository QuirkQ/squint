//! A small GitHub client: GraphQL for pull requests, conversations and checks,
//! REST for the per-file patches and Actions jobs and logs, which GraphQL does
//! not expose.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use shared::diff::parse_patch;
use shared::{
    Actor, CheckRun, CheckState, CheckSuite, Checks, Comment, FileDiff, FileStatus, Inbox, Job,
    JobStep, NewComment, PullDetail, PullSummary, Review, ReviewDecision, ReviewState,
    ReviewThread, Role, RunState, Side, StatusCheck, TimelineItem, WorkflowRun,
};

use crate::error::{Error, Result};

const API: &str = "https://api.github.com";
/// Search pages of 50; caps the inbox at 300 pull requests per query.
const MAX_SEARCH_PAGES: usize = 6;
/// The files endpoint returns at most 3000 files, 100 per page.
const MAX_FILE_PAGES: usize = 30;
/// Job logs beyond this are left to GitHub; the viewer would crawl.
const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;
const LOG_TIMEOUT: Duration = Duration::from_secs(120);
/// GitHub's 502s and 503s are usually gone a moment later.
const ATTEMPTS: u32 = 4;
/// The first retry waits this long, each next one twice as long.
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// A server asking to wait longer than this is treated as down for now.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    token: Arc<str>,
    api: Arc<str>,
    retry_delay: Duration,
}

impl GitHub {
    pub fn new(token: String) -> Result<Self> {
        Self::with_api(token, API, RETRY_DELAY)
    }

    fn with_api(token: String, api: &str, retry_delay: Duration) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        let http = reqwest::Client::builder()
            .user_agent(concat!("squint/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(GitHub {
            http,
            token: token.into(),
            api: api.into(),
            retry_delay,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The login the token belongs to; doubles as a cheap validity check.
    pub async fn viewer_login(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Data {
            viewer: Login,
        }
        let data: Data = self.graphql("{ viewer { login } }", json!({})).await?;
        Ok(data.viewer.login)
    }

    pub async fn inbox(&self) -> Result<Inbox> {
        // `involves:` covers author, assignee, mentions and comments, but not
        // pending review requests, so those need a query of their own.
        let base = "is:pr is:open archived:false sort:updated-desc";
        let involves = format!("{base} involves:@me");
        let review_requested = format!("{base} review-requested:@me");
        let ((viewer, involved, complete_a), (_, requested, complete_b)) =
            tokio::try_join!(self.search(&involves), self.search(&review_requested))?;

        let requested_keys: HashSet<_> = requested.iter().map(RawPull::key).collect();
        let mut seen = HashSet::new();
        let mut pulls: Vec<PullSummary> = involved
            .into_iter()
            .chain(requested)
            .filter(|pr| seen.insert(pr.key()))
            .map(|pr| pr.into_summary(&viewer, &requested_keys))
            .collect();
        pulls.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(Inbox {
            viewer,
            pulls,
            complete: complete_a && complete_b,
        })
    }

    /// Returns the viewer, the pull requests, and whether that is all of them
    /// (no errors, no pages left unread).
    async fn search(&self, query: &str) -> Result<(String, Vec<RawPull>, bool)> {
        #[derive(Deserialize)]
        struct Data {
            viewer: Login,
            search: Connection<Option<RawPull>>,
        }

        let mut pulls = Vec::new();
        let mut after: Option<String> = None;
        let mut viewer = String::new();
        let mut complete = false;
        for _ in 0..MAX_SEARCH_PAGES {
            let (data, errors): (Data, _) = self
                .graphql_partial(SEARCH_QUERY, json!({ "q": query, "after": after }))
                .await?;
            viewer = data.viewer.login;
            let all_present = data.search.nodes.iter().all(Option::is_some);
            pulls.extend(data.search.nodes.into_iter().flatten());
            if !errors.is_empty() || !all_present {
                return Ok((viewer, pulls, false));
            }
            match data.search.page_info {
                Some(PageInfo {
                    has_next_page: true,
                    end_cursor,
                }) => after = end_cursor,
                _ => {
                    complete = true;
                    break;
                }
            }
        }
        Ok((viewer, pulls, complete))
    }

    pub async fn detail(&self, repo: &str, number: u32) -> Result<PullDetail> {
        #[derive(Deserialize)]
        struct Data {
            repository: Option<Repo>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Repo {
            pull_request: Option<RawDetail>,
        }

        let (owner, name) = split_repo(repo)?;
        let data: Data = self
            .graphql(
                DETAIL_QUERY,
                json!({ "owner": owner, "name": name, "number": number }),
            )
            .await?;
        let pr = data
            .repository
            .and_then(|r| r.pull_request)
            .ok_or_else(|| Error::msg(format!("{repo}#{number} was not found")))?;
        Ok(pr.into_detail())
    }

    pub async fn files(&self, repo: &str, number: u32) -> Result<Vec<FileDiff>> {
        #[derive(Deserialize)]
        struct RawFile {
            filename: String,
            previous_filename: Option<String>,
            status: FileStatus,
            additions: u32,
            deletions: u32,
            patch: Option<String>,
        }

        split_repo(repo)?;
        let mut files = Vec::new();
        for page in 1..=MAX_FILE_PAGES {
            let url = format!(
                "{}/repos/{repo}/pulls/{number}/files?per_page=100&page={page}",
                self.api
            );
            let batch: Vec<RawFile> = self.send(|| self.http.get(&url)).await?.json().await?;
            let last = batch.len() < 100;
            files.extend(batch.into_iter().map(|f| {
                let unchanged_rename =
                    f.status == FileStatus::Renamed && f.additions + f.deletions == 0;
                FileDiff {
                    patch_missing: f.patch.is_none() && !unchanged_rename,
                    hunks: f.patch.as_deref().map(parse_patch).unwrap_or_default(),
                    path: f.filename,
                    previous_path: f.previous_filename,
                    status: f.status,
                    additions: f.additions,
                    deletions: f.deletions,
                }
            }));
            if last {
                break;
            }
        }
        Ok(files)
    }

    /// The checks on the pull request's head commit.
    pub async fn checks(&self, repo: &str, number: u32) -> Result<Checks> {
        #[derive(Deserialize)]
        struct Data {
            repository: Option<Repo>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Repo {
            pull_request: Option<Pull>,
        }
        #[derive(Deserialize)]
        struct Pull {
            commits: Connection<HeadCommit>,
        }
        #[derive(Deserialize)]
        struct HeadCommit {
            commit: RawCommitChecks,
        }

        let (owner, name) = split_repo(repo)?;
        let data: Data = self
            .graphql(
                CHECKS_QUERY,
                json!({ "owner": owner, "name": name, "number": number }),
            )
            .await?;
        let commit = data
            .repository
            .and_then(|r| r.pull_request)
            .ok_or_else(|| Error::msg(format!("{repo}#{number} was not found")))?
            .commits
            .nodes
            .into_iter()
            .next();
        Ok(commit.map(|c| c.commit.into_checks()).unwrap_or_default())
    }

    /// A workflow run's jobs, with their steps, across all its attempts.
    pub async fn run_jobs(&self, repo: &str, run: u64) -> Result<Vec<Job>> {
        #[derive(Deserialize)]
        struct Page {
            jobs: Vec<RawJob>,
        }

        split_repo(repo)?;
        let url = format!(
            "{}/repos/{repo}/actions/runs/{run}/jobs?filter=all&per_page=100",
            self.api
        );
        let page: Page = self.send(|| self.http.get(&url)).await?.json().await?;
        Ok(page.jobs.into_iter().map(RawJob::into_job).collect())
    }

    /// A finished job's plain-text log. GitHub redirects to a short-lived
    /// download link, and reqwest drops the token when following it.
    pub async fn job_log(&self, repo: &str, job: u64) -> Result<String> {
        split_repo(repo)?;
        let url = format!("{}/repos/{repo}/actions/jobs/{job}/logs", self.api);
        let response = self
            .send(|| self.http.get(&url).timeout(LOG_TIMEOUT))
            .await?;
        if response.content_length().unwrap_or(0) > MAX_LOG_BYTES {
            return Err(Error::msg(
                "This log is too large to show here; open it on GitHub.",
            ));
        }
        let bytes = response.bytes().await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Approves the pull request at `commit`, the head that was reviewed,
    /// with `comment` as the review's body if there is one.
    pub async fn approve(
        &self,
        repo: &str,
        number: u32,
        commit: &str,
        comment: &str,
    ) -> Result<()> {
        split_repo(repo)?;
        let url = format!("{}/repos/{repo}/pulls/{number}/reviews", self.api);
        let mut review = json!({ "event": "APPROVE", "commit_id": commit });
        if !comment.trim().is_empty() {
            review["body"] = comment.trim().into();
        }
        self.send_once(self.http.post(&url).json(&review)).await?;
        Ok(())
    }

    /// Closes the pull request without merging, then leaves `comment` on it
    /// if there is one. In that order a retry after a failure never posts the
    /// comment twice, since closing a closed PR changes nothing.
    pub async fn close(&self, repo: &str, number: u32, comment: &str) -> Result<()> {
        split_repo(repo)?;
        let url = format!("{}/repos/{repo}/pulls/{number}", self.api);
        let state = json!({ "state": "closed" });
        self.send(|| self.http.patch(&url).json(&state)).await?;
        if !comment.trim().is_empty() {
            let url = format!("{}/repos/{repo}/issues/{number}/comments", self.api);
            let body = json!({ "body": comment.trim() });
            self.send_once(self.http.post(&url).json(&body)).await?;
        }
        Ok(())
    }

    /// Posts a review comment on a line, or a range of lines, of the diff.
    /// It is published at once, as GitHub's *Add single comment* does.
    pub async fn add_comment(&self, repo: &str, number: u32, comment: &NewComment) -> Result<()> {
        split_repo(repo)?;
        let url = format!("{}/repos/{repo}/pulls/{number}/comments", self.api);
        self.send_once(self.http.post(&url).json(&comment_request(comment)))
            .await
            .map_err(explain_pending_review)?;
        Ok(())
    }

    /// Replies to a review thread, given the REST id of its first comment.
    pub async fn reply(&self, repo: &str, number: u32, comment_id: u64, body: &str) -> Result<()> {
        split_repo(repo)?;
        let url = format!(
            "{}/repos/{repo}/pulls/{number}/comments/{comment_id}/replies",
            self.api
        );
        let body = json!({ "body": body });
        self.send_once(self.http.post(&url).json(&body))
            .await
            .map_err(explain_pending_review)?;
        Ok(())
    }

    /// Resolves a review thread, or unresolves it; REST can do neither.
    pub async fn set_resolved(&self, thread: &str, resolved: bool) -> Result<()> {
        let mutation = if resolved {
            RESOLVE_MUTATION
        } else {
            UNRESOLVE_MUTATION
        };
        let (_, errors): (serde_json::Value, _) = self
            .graphql_partial(mutation, json!({ "thread": thread }))
            .await?;
        match errors.is_empty() {
            true => Ok(()),
            false => Err(Error::msg(errors.join("; "))),
        }
    }

    async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        Ok(self.graphql_partial(query, variables).await?.0)
    }

    /// Also returns the errors GitHub reported alongside the data.
    async fn graphql_partial<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<(T, Vec<String>)> {
        #[derive(Deserialize)]
        struct Response<T> {
            data: Option<T>,
            #[serde(default)]
            errors: Vec<GraphqlError>,
        }
        #[derive(Deserialize)]
        struct GraphqlError {
            message: String,
        }

        let url = format!("{}/graphql", self.api);
        let request = json!({ "query": query, "variables": variables });
        let body: Response<T> = self
            .send(|| self.http.post(&url).json(&request))
            .await?
            .json()
            .await?;
        let errors: Vec<String> = body.errors.into_iter().map(|e| e.message).collect();
        match body.data {
            // Partial data is normal, e.g. when one org enforces SSO the token
            // is not authorised for; show what we can.
            Some(data) => Ok((data, errors)),
            None => Err(Error::msg(errors.join("; "))),
        }
    }

    /// Sends an authenticated request, retrying what's usually momentary:
    /// GitHub's 5xx answers (its 502s in particular), timeouts and dropped
    /// connections. Rate limits are never retried, since that only makes
    /// them worse; the error says when to try again instead.
    async fn send(
        &self,
        request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut attempt = 1;
        loop {
            let sent = request().bearer_auth(&*self.token).send().await;
            let wait = match &sent {
                Ok(response) if response.status().is_success() => None,
                Ok(response) => retry_wait(response.status(), response.headers()),
                // A request that already took the full timeout gets one more
                // go, not three.
                Err(error) if error.is_timeout() => (attempt == 1).then_some(Duration::ZERO),
                Err(error) => (error.is_connect() || error.is_request()).then_some(Duration::ZERO),
            };
            match wait {
                Some(wait) if attempt < ATTEMPTS => {
                    let backoff = self.retry_delay * 2u32.pow(attempt - 1);
                    tokio::time::sleep(wait.max(backoff)).await;
                    attempt += 1;
                }
                _ => return check(sent?, attempt).await,
            }
        }
    }

    /// Sends a request that adds something, exactly once: a 502 can arrive
    /// after GitHub has done it, and a retry would post it a second time.
    async fn send_once(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        check(request.bearer_auth(&*self.token).send().await?, 1).await
    }
}

/// How long to wait before retrying a failed answer, or `None` when retrying
/// won't help.
fn retry_wait(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if !matches!(status.as_u16(), 500 | 502 | 503 | 504) {
        return None;
    }
    match seconds(headers, RETRY_AFTER.as_str()).map(Duration::from_secs) {
        Some(wait) if wait > MAX_RETRY_AFTER => None,
        Some(wait) => Some(wait),
        None => Some(Duration::ZERO),
    }
}

fn seconds(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// When GitHub says to stop for now: 429, or a 403 with the primary limit
/// used up (`x-ratelimit-remaining: 0`) or a secondary limit (`retry-after`,
/// or a message saying so). Returns how long until it lifts, when known.
fn rate_limited(
    status: StatusCode,
    headers: &HeaderMap,
    message: &str,
) -> Option<Option<Duration>> {
    let limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && (seconds(headers, "x-ratelimit-remaining") == Some(0)
                || headers.contains_key(RETRY_AFTER)
                || message.to_lowercase().contains("rate limit")));
    if !limited {
        return None;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let wait = seconds(headers, RETRY_AFTER.as_str())
        .or_else(|| seconds(headers, "x-ratelimit-reset").map(|reset| reset.saturating_sub(now)));
    Some(wait.map(Duration::from_secs))
}

/// The REST body for a new review comment. A range names its first line
/// apart; a single line has none.
fn comment_request(comment: &NewComment) -> serde_json::Value {
    let side = |side: Side| match side {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    };
    let mut request = json!({
        "body": comment.body,
        "commit_id": comment.commit,
        "path": comment.path,
        "line": comment.end.line,
        "side": side(comment.end.side),
    });
    if comment.start != comment.end {
        request["start_line"] = comment.start.line.into();
        request["start_side"] = side(comment.start.side).into();
    }
    request
}

/// GitHub refuses single comments and replies while you have a review
/// pending on the pull request, saying only that `user_id can only have one
/// pending review`.
fn explain_pending_review(error: Error) -> Error {
    if error.to_string().contains("one pending review") {
        Error::msg(
            "You have a review pending on this pull request, and until it's \
             submitted GitHub takes no single comments from you. Submit or \
             discard it on GitHub, then try again.",
        )
    } else {
        error
    }
}

/// `in 12 min`, `in 40 s`.
fn wait_text(wait: Duration) -> String {
    match wait.as_secs() {
        s if s < 60 => format!("in {} s", s.max(1)),
        s => format!("in {} min", s.div_ceil(60)),
    }
}

async fn check(response: reqwest::Response, attempts: u32) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    #[derive(Deserialize)]
    struct Body {
        message: String,
        /// A 422's reasons, as strings or as objects with a message.
        #[serde(default)]
        errors: Vec<serde_json::Value>,
    }
    let headers = response.headers().clone();
    let detail = response
        .json::<Body>()
        .await
        .map(|b| {
            let reasons: Vec<&str> = b
                .errors
                .iter()
                .filter_map(|e| e.as_str().or_else(|| e.get("message")?.as_str()))
                .collect();
            if reasons.is_empty() {
                b.message
            } else {
                reasons.join("; ")
            }
        })
        .unwrap_or_default();
    if let Some(wait) = rate_limited(status, &headers, &detail) {
        let when = wait.map_or("later".into(), wait_text);
        return Err(Error::msg(format!(
            "GitHub's rate limit is reached; refreshing again works {when}."
        )));
    }
    let mut message = match status.as_u16() {
        401 => "GitHub rejected the token. Add a new one in Settings.".to_string(),
        _ if detail.is_empty() => format!("GitHub returned {status}"),
        _ => format!("GitHub returned {status}: {detail}"),
    };
    if attempts > 1 {
        message.push_str(&format!(" (tried {attempts} times)"));
    }
    Err(Error::msg(message))
}

/// Validates `owner/name`, which ends up in URLs and filesystem paths.
pub fn split_repo(repo: &str) -> Result<(&str, &str)> {
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok((owner, name)),
        _ => Err(Error::msg(format!("Not a repository name: {repo:?}"))),
    }
}

const SEARCH_QUERY: &str = "query($q: String!, $after: String) {
  viewer { login }
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title url isDraft updatedAt headRefName baseRefName headRefOid isCrossRepository
        additions deletions changedFiles reviewDecision
        author { login avatarUrl(size: 64) }
        repository { nameWithOwner }
        assignees(first: 10) { nodes { login } }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
        comments(last: 1) { nodes { at: createdAt } }
        reviews(last: 5) { nodes { at: submittedAt } }
      }
    }
  }
}";

const DETAIL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      bodyHTML createdAt url headRefOid
      author { login avatarUrl(size: 64) }
      comments(first: 100) {
        nodes { author { login avatarUrl(size: 64) } bodyHTML createdAt url }
      }
      reviews(first: 100) {
        nodes { author { login avatarUrl(size: 64) } state bodyHTML submittedAt createdAt url }
      }
      reviewThreads(first: 100) {
        nodes {
          id isResolved isOutdated path line startLine diffSide startDiffSide subjectType
          viewerCanReply viewerCanResolve viewerCanUnresolve
          comments(first: 50) {
            nodes { databaseId author { login avatarUrl(size: 64) } bodyHTML createdAt url }
          }
        }
      }
    }
  }
}";

const RESOLVE_MUTATION: &str = "mutation($thread: ID!) {
  resolveReviewThread(input: { threadId: $thread }) { thread { isResolved } }
}";

const UNRESOLVE_MUTATION: &str = "mutation($thread: ID!) {
  unresolveReviewThread(input: { threadId: $thread }) { thread { isResolved } }
}";

const CHECKS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      commits(last: 1) {
        nodes {
          commit {
            status { contexts { context state description targetUrl } }
            statusCheckRollup {
              contexts(first: 100) { totalCount nodes { ... on CheckRun { databaseId } } }
            }
            checkSuites(first: 50) {
              nodes {
                status conclusion
                app { name }
                workflowRun { databaseId runNumber event url createdAt workflow { name } }
                checkRuns(first: 100, filterBy: { checkType: LATEST }) {
                  nodes { databaseId name status conclusion startedAt completedAt detailsUrl }
                }
              }
            }
          }
        }
      }
    }
  }
}";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: Option<PageInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawActor {
    login: String,
    avatar_url: String,
}

impl From<RawActor> for Actor {
    fn from(a: RawActor) -> Self {
        Actor {
            login: a.login,
            avatar_url: a.avatar_url,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoName {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPull {
    number: u32,
    title: String,
    url: String,
    is_draft: bool,
    updated_at: String,
    head_ref_name: String,
    base_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
    additions: u32,
    deletions: u32,
    changed_files: u32,
    review_decision: Option<String>,
    author: Option<RawActor>,
    repository: RepoName,
    assignees: Connection<Login>,
    commits: Connection<CommitNode>,
    comments: Connection<Stamp>,
    /// Reviews (which also carry every inline comment and thread reply) are
    /// listed in the order they were started, not submitted, so the newest
    /// may be a few from the end.
    reviews: Connection<Stamp>,
}

#[derive(Deserialize)]
struct Stamp {
    /// `None` for a review that is still pending.
    at: Option<String>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: CommitRollup,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitRollup {
    status_check_rollup: Option<StateField>,
}

#[derive(Deserialize)]
struct StateField {
    state: String,
}

impl RawPull {
    fn key(&self) -> (String, u32) {
        (self.repository.name_with_owner.clone(), self.number)
    }

    fn into_summary(self, viewer: &str, requested: &HashSet<(String, u32)>) -> PullSummary {
        let mut roles = Vec::new();
        if self.author.as_ref().is_some_and(|a| a.login == viewer) {
            roles.push(Role::Author);
        }
        if requested.contains(&self.key()) {
            roles.push(Role::ReviewRequested);
        }
        if self.assignees.nodes.iter().any(|a| a.login == viewer) {
            roles.push(Role::Assignee);
        }
        if roles.is_empty() {
            roles.push(Role::Involved);
        }

        let checks = self
            .commits
            .nodes
            .into_iter()
            .next()
            .and_then(|n| n.commit.status_check_rollup)
            .map(|s| match s.state.as_str() {
                "SUCCESS" => CheckState::Success,
                "FAILURE" | "ERROR" => CheckState::Failure,
                _ => CheckState::Pending,
            });
        let review_decision = self.review_decision.as_deref().and_then(|d| match d {
            "APPROVED" => Some(ReviewDecision::Approved),
            "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
            "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
            _ => None,
        });
        // GitHub timestamps are all UTC `…Z`, so they sort as strings.
        let updated_at = self
            .comments
            .nodes
            .into_iter()
            .chain(self.reviews.nodes)
            .filter_map(|stamp| stamp.at)
            .chain([self.updated_at])
            .max()
            .unwrap_or_default();

        PullSummary {
            repo: self.repository.name_with_owner,
            number: self.number,
            title: self.title,
            url: self.url,
            author: self.author.map(Actor::from),
            is_draft: self.is_draft,
            updated_at,
            head_ref: self.head_ref_name,
            base_ref: self.base_ref_name,
            head_sha: self.head_ref_oid,
            is_fork: self.is_cross_repository,
            additions: self.additions,
            deletions: self.deletions,
            changed_files: self.changed_files,
            review_decision,
            checks,
            roles,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    /// Only asked for on review comments, which replies answer.
    #[serde(default)]
    database_id: Option<u64>,
    author: Option<RawActor>,
    #[serde(rename = "bodyHTML")]
    body_html: String,
    created_at: String,
    url: String,
}

impl From<RawComment> for Comment {
    fn from(c: RawComment) -> Self {
        Comment {
            author: c.author.map(Actor::from),
            body_html: c.body_html,
            created_at: c.created_at,
            url: c.url,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReview {
    author: Option<RawActor>,
    state: String,
    #[serde(rename = "bodyHTML")]
    body_html: String,
    submitted_at: Option<String>,
    created_at: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    path: String,
    line: Option<u32>,
    /// Equal to `line` on a single line, where `start_diff_side` is null.
    start_line: Option<u32>,
    diff_side: String,
    start_diff_side: Option<String>,
    /// `LINE` or `FILE`.
    subject_type: String,
    viewer_can_reply: bool,
    viewer_can_resolve: bool,
    viewer_can_unresolve: bool,
    comments: Connection<RawComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetail {
    #[serde(rename = "bodyHTML")]
    body_html: String,
    created_at: String,
    url: String,
    head_ref_oid: String,
    author: Option<RawActor>,
    comments: Connection<RawComment>,
    reviews: Connection<RawReview>,
    review_threads: Connection<RawThread>,
}

impl RawDetail {
    fn into_detail(self) -> PullDetail {
        let comments = self
            .comments
            .nodes
            .into_iter()
            .map(|c| TimelineItem::Comment(c.into()));

        let reviews = self.reviews.nodes.into_iter().filter_map(|r| {
            let state = match r.state.as_str() {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "DISMISSED" => ReviewState::Dismissed,
                "PENDING" => ReviewState::Pending,
                _ => ReviewState::Commented,
            };
            // A bodiless "commented" review only wraps inline comments, which
            // already appear as threads.
            if state == ReviewState::Commented && r.body_html.trim().is_empty() {
                return None;
            }
            Some(TimelineItem::Review(Review {
                author: r.author.map(Actor::from),
                state,
                body_html: r.body_html,
                submitted_at: r.submitted_at.unwrap_or(r.created_at),
                url: r.url,
            }))
        });

        let threads = self.review_threads.nodes.into_iter().map(|t| {
            // A comment on the whole file comes with line 1.
            let line = t.line.filter(|_| t.subject_type != "FILE");
            let start_line = t
                .start_line
                .filter(|_| line.is_some() && t.start_diff_side.is_some());
            TimelineItem::Thread(ReviewThread {
                id: t.id,
                path: t.path,
                line,
                start_line,
                side: if t.diff_side == "LEFT" {
                    Side::Old
                } else {
                    Side::New
                },
                is_resolved: t.is_resolved,
                is_outdated: t.is_outdated,
                root_comment_id: t.comments.nodes.first().and_then(|c| c.database_id),
                can_reply: t.viewer_can_reply,
                can_resolve: t.viewer_can_resolve,
                can_unresolve: t.viewer_can_unresolve,
                comments: t.comments.nodes.into_iter().map(Comment::from).collect(),
            })
        });

        let mut timeline: Vec<TimelineItem> = comments.chain(reviews).chain(threads).collect();
        // GitHub timestamps are all UTC `…Z`, so they sort as strings.
        timeline.sort_by(|a, b| a.timestamp().cmp(b.timestamp()));

        PullDetail {
            description: Comment {
                author: self.author.map(Actor::from),
                body_html: self.body_html,
                created_at: self.created_at,
                url: self.url,
            },
            head_sha: self.head_ref_oid,
            timeline,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommitChecks {
    status: Option<RawStatus>,
    status_check_rollup: Option<RawRollup>,
    check_suites: Connection<RawSuite>,
}

/// The checks GitHub counts towards the commit's status, which is what the
/// pull request shows.
#[derive(Deserialize)]
struct RawRollup {
    contexts: RawRollupContexts,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRollupContexts {
    total_count: usize,
    nodes: Vec<RawRollupContext>,
}

/// A check run's id; commit statuses have none.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRollupContext {
    #[serde(default)]
    database_id: Option<u64>,
}

#[derive(Deserialize)]
struct RawStatus {
    contexts: Vec<RawContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawContext {
    context: String,
    state: String,
    description: Option<String>,
    target_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSuite {
    status: String,
    conclusion: Option<String>,
    app: Option<AppName>,
    workflow_run: Option<RawWorkflowRun>,
    check_runs: Option<Connection<RawCheckRun>>,
}

#[derive(Deserialize)]
struct AppName {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWorkflowRun {
    database_id: Option<u64>,
    run_number: u32,
    event: String,
    url: String,
    created_at: Option<String>,
    workflow: AppName,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheckRun {
    database_id: Option<u64>,
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    details_url: Option<String>,
}

impl RawCommitChecks {
    fn into_checks(self) -> Checks {
        // The PR's checks are the ones GitHub rolls up into the commit's
        // status. Anything else, such as a deployment started by hand, is
        // listed apart. A rollup too long to read whole counts everything.
        let rollup: Option<HashSet<u64>> = self.status_check_rollup.and_then(|rollup| {
            let contexts = rollup.contexts;
            (contexts.nodes.len() >= contexts.total_count).then(|| {
                contexts
                    .nodes
                    .into_iter()
                    .filter_map(|c| c.database_id)
                    .collect()
            })
        });
        let mut suites = Vec::new();
        let mut others = Vec::new();
        for (created_at, suite) in self
            .check_suites
            .nodes
            .into_iter()
            .filter_map(RawSuite::into_suite)
        {
            let on_pr = rollup
                .as_ref()
                .is_none_or(|ids| suite.checks.iter().any(|c| ids.contains(&c.id)));
            if on_pr {
                suites.push(suite);
            } else {
                others.push((created_at, suite));
            }
        }
        // Each edit of a PR re-runs its pull_request workflows; GitHub shows
        // only the latest run of each.
        let mut latest: HashMap<(String, String), u32> = HashMap::new();
        for suite in &suites {
            if let Some(run) = &suite.run {
                let number = latest
                    .entry((suite.name.clone(), run.event.clone()))
                    .or_default();
                *number = (*number).max(run.number);
            }
        }
        suites.retain(|suite| {
            suite
                .run
                .as_ref()
                .is_none_or(|run| latest[&(suite.name.clone(), run.event.clone())] == run.number)
        });
        suites.sort_by_key(|s| s.state.rank());
        // GitHub timestamps are all UTC `…Z`, so they sort as strings.
        others.sort_by(|a, b| b.0.cmp(&a.0));

        let mut statuses: Vec<StatusCheck> = self
            .status
            .map(|s| s.contexts)
            .unwrap_or_default()
            .into_iter()
            .map(|c| StatusCheck {
                state: RunState::from_status(&c.state),
                context: c.context,
                description: c.description.filter(|d| !d.is_empty()),
                url: c.target_url.filter(|u| !u.is_empty()),
            })
            .collect();
        statuses.sort_by_key(|s| s.state.rank());
        Checks {
            suites,
            statuses,
            others: others.into_iter().map(|(_, suite)| suite).collect(),
        }
    }
}

impl RawSuite {
    /// The suite, with when its workflow run started. Apps that are
    /// installed but never ran leave empty, queued suites, which are dropped.
    fn into_suite(self) -> Option<(Option<String>, CheckSuite)> {
        let mut checks: Vec<CheckRun> = self
            .check_runs?
            .nodes
            .into_iter()
            .filter_map(|run| {
                Some(CheckRun {
                    id: run.database_id?,
                    name: run.name,
                    state: RunState::from_github(&run.status, run.conclusion.as_deref()),
                    started_at: run.started_at,
                    completed_at: run.completed_at,
                    url: run.details_url,
                })
            })
            .collect();
        if checks.is_empty() {
            return None;
        }
        checks.sort_by_key(|c| c.state.rank());
        let run = self.workflow_run.as_ref().and_then(|run| {
            Some(WorkflowRun {
                id: run.database_id?,
                number: run.run_number,
                event: run.event.clone(),
                url: run.url.clone(),
            })
        });
        let (name, created_at) = match (self.workflow_run, self.app) {
            (Some(run), _) => (run.workflow.name, run.created_at),
            (None, Some(app)) => (app.name, None),
            (None, None) => ("Checks".into(), None),
        };
        let suite = CheckSuite {
            name,
            state: RunState::from_github(&self.status, self.conclusion.as_deref()),
            run,
            checks,
        };
        Some((created_at, suite))
    }
}

#[derive(Deserialize)]
struct RawJob {
    id: u64,
    run_id: u64,
    run_attempt: Option<u32>,
    name: String,
    workflow_name: Option<String>,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    html_url: Option<String>,
    steps: Option<Vec<RawStep>>,
}

#[derive(Deserialize)]
struct RawStep {
    number: u32,
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

impl RawJob {
    fn into_job(self) -> Job {
        Job {
            id: self.id,
            run_id: self.run_id,
            run_attempt: self.run_attempt.unwrap_or(1),
            name: self.name,
            workflow: self.workflow_name.unwrap_or_default(),
            state: RunState::from_github(&self.status, self.conclusion.as_deref()),
            started_at: self.started_at,
            completed_at: self.completed_at,
            url: self.html_url.unwrap_or_default(),
            steps: self
                .steps
                .unwrap_or_default()
                .into_iter()
                .map(|s| JobStep {
                    number: s.number,
                    name: s.name,
                    state: RunState::from_github(&s.status, s.conclusion.as_deref()),
                    started_at: s.started_at,
                    completed_at: s.completed_at,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A stand-in for api.github.com that answers each request with the next
    /// canned response (`status line`, extra headers, body), counts them, and
    /// keeps each one's request line and body.
    struct Server {
        url: String,
        requests: Arc<Mutex<u32>>,
        received: Arc<Mutex<Vec<(String, String)>>>,
    }

    fn serve(responses: Vec<(&'static str, &'static str, &'static str)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(0));
        let count = requests.clone();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        std::thread::spawn(move || {
            for (stream, (status, headers, body)) in listener.incoming().zip(responses) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut sent = vec![0; length];
                reader.read_exact(&mut sent).unwrap();
                *count.lock().unwrap() += 1;
                log.lock().unwrap().push((
                    request_line.trim_end().to_string(),
                    String::from_utf8(sent).unwrap(),
                ));
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        Server {
            url,
            requests,
            received,
        }
    }

    /// The request line and JSON body of the `n`th request.
    fn request(server: &Server, n: usize) -> (String, serde_json::Value) {
        let (line, body) = server.received.lock().unwrap()[n].clone();
        let body = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&body).unwrap()
        };
        (line, body)
    }

    fn client(server: &Server) -> GitHub {
        GitHub::with_api("t".into(), &server.url, Duration::from_millis(1)).unwrap()
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    const VIEWER: &str = r#"{"data":{"viewer":{"login":"me"}}}"#;

    #[test]
    fn retries_gateway_errors() {
        let server = serve(vec![
            ("502 Bad Gateway", "", "<html>bad gateway</html>"),
            ("503 Service Unavailable", "Retry-After: 0\r\n", ""),
            ("200 OK", "", VIEWER),
        ]);
        assert_eq!(block_on(client(&server).viewer_login()).unwrap(), "me");
        assert_eq!(*server.requests.lock().unwrap(), 3);
    }

    #[test]
    fn gives_up_after_a_few_tries() {
        let bad = ("502 Bad Gateway", "", "");
        let server = serve(vec![bad; 5]);
        let error = block_on(client(&server).viewer_login()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub returned 502 Bad Gateway (tried 4 times)"
        );
        assert_eq!(*server.requests.lock().unwrap(), ATTEMPTS);
    }

    #[test]
    fn never_retries_rate_limits() {
        for (status, headers, body) in [
            ("429 Too Many Requests", "Retry-After: 90\r\n", ""),
            (
                "403 Forbidden",
                "x-ratelimit-remaining: 0\r\nx-ratelimit-reset: 0\r\n",
                r#"{"message":"API rate limit exceeded"}"#,
            ),
            (
                "403 Forbidden",
                "",
                r#"{"message":"You have exceeded a secondary rate limit."}"#,
            ),
        ] {
            let server = serve(vec![(status, headers, body), ("200 OK", "", VIEWER)]);
            let error = block_on(client(&server).viewer_login()).unwrap_err();
            assert!(
                error.to_string().contains("rate limit"),
                "{status}: {error}"
            );
            assert_eq!(*server.requests.lock().unwrap(), 1, "{status}");
        }
        let server = serve(vec![("429 Too Many Requests", "Retry-After: 90\r\n", "")]);
        let error = block_on(client(&server).viewer_login()).unwrap_err();
        assert!(error.to_string().ends_with("works in 2 min."), "{error}");
    }

    #[test]
    fn other_errors_fail_at_once() {
        let server = serve(vec![
            ("404 Not Found", "", r#"{"message":"Not Found"}"#),
            ("200 OK", "", VIEWER),
        ]);
        let error = block_on(client(&server).viewer_login()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub returned 404 Not Found: Not Found"
        );
        assert_eq!(*server.requests.lock().unwrap(), 1);
    }

    #[test]
    fn never_repeats_a_review() {
        let server = serve(vec![("502 Bad Gateway", "", ""), ("200 OK", "", "{}")]);
        assert!(block_on(client(&server).approve("o/r", 7, "abc", "LGTM")).is_err());
        assert_eq!(*server.requests.lock().unwrap(), 1);
    }

    #[test]
    fn explains_validation_errors() {
        let server = serve(vec![(
            "422 Unprocessable Entity",
            "",
            r#"{"message":"Unprocessable Entity","errors":["Review Can not approve your own pull request"]}"#,
        )]);
        let error = block_on(client(&server).approve("o/r", 7, "abc", "")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub returned 422 Unprocessable Entity: Review Can not approve your own pull request"
        );
    }

    #[test]
    fn a_long_retry_after_is_not_waited_out() {
        let headers = |s: &'static str| {
            let mut map = HeaderMap::new();
            map.insert(RETRY_AFTER, HeaderValue::from_static(s));
            map
        };
        assert_eq!(
            retry_wait(StatusCode::BAD_GATEWAY, &HeaderMap::new()),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_wait(StatusCode::SERVICE_UNAVAILABLE, &headers("3")),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_wait(StatusCode::SERVICE_UNAVAILABLE, &headers("600")),
            None
        );
        assert_eq!(
            retry_wait(StatusCode::NOT_IMPLEMENTED, &HeaderMap::new()),
            None
        );
        assert_eq!(wait_text(Duration::from_secs(5)), "in 5 s");
        assert_eq!(wait_text(Duration::from_secs(61)), "in 2 min");
    }

    fn suite(
        name: &str,
        run: (u64, u32, &str, &str),
        checks: serde_json::Value,
    ) -> serde_json::Value {
        let (id, number, event, created_at) = run;
        json!({
            "status": "COMPLETED", "conclusion": "SUCCESS", "app": { "name": "GitHub Actions" },
            "workflowRun": { "databaseId": id, "runNumber": number, "event": event,
                             "url": format!("https://github.com/o/r/actions/runs/{id}"),
                             "createdAt": created_at, "workflow": { "name": name } },
            "checkRuns": { "nodes": checks }
        })
    }

    fn check_run(id: u64, name: &str, conclusion: &str) -> serde_json::Value {
        json!({ "databaseId": id, "name": name, "status": "COMPLETED", "conclusion": conclusion,
                "startedAt": null, "completedAt": null, "detailsUrl": null })
    }

    #[test]
    fn sorts_the_prs_checks_from_the_commits_other_runs() {
        let raw: RawCommitChecks = serde_json::from_value(json!({
            "status": { "contexts": [
                { "context": "ci/legacy", "state": "SUCCESS", "description": "", "targetUrl": "https://ci" }
            ] },
            "statusCheckRollup": { "contexts": { "totalCount": 6, "nodes": [
                { "databaseId": 1 }, { "databaseId": 2 }, { "databaseId": 3 },
                { "databaseId": 4 }, { "databaseId": 5 }, {}
            ] } },
            "checkSuites": { "nodes": [
                { "status": "QUEUED", "conclusion": null, "app": { "name": "Cursor" },
                  "workflowRun": null, "checkRuns": { "nodes": [] } },
                suite("Validate PR", (10, 3400, "pull_request", "2026-10-05T10:37:34Z"),
                      json!([check_run(1, "Semantic PR check", "SUCCESS")])),
                suite("Validate PR", (11, 3402, "pull_request", "2026-10-05T10:53:57Z"),
                      json!([check_run(2, "Semantic PR check", "SUCCESS")])),
                { "status": "COMPLETED", "conclusion": "FAILURE", "app": { "name": "Aikido" },
                  "workflowRun": null,
                  "checkRuns": { "nodes": [
                    { "databaseId": 3, "name": "Scan", "status": "COMPLETED", "conclusion": "SUCCESS",
                      "startedAt": null, "completedAt": null, "detailsUrl": "https://aikido" },
                    { "databaseId": 4, "name": "Secrets", "status": "COMPLETED", "conclusion": "FAILURE",
                      "startedAt": null, "completedAt": null, "detailsUrl": "https://aikido" }
                  ] } },
                suite("Build", (12, 2799, "push", "2026-10-05T10:37:32Z"),
                      json!([check_run(5, "Test suite", "SUCCESS")])),
                suite("Deploy", (13, 1051, "workflow_dispatch", "2026-10-05T12:43:05Z"),
                      json!([check_run(6, "Deploy to development", "FAILURE")])),
                suite("Deploy", (14, 1052, "workflow_dispatch", "2026-10-05T12:49:07Z"),
                      json!([check_run(7, "Deploy to development", "SUCCESS")]))
            ] }
        }))
        .unwrap();
        let checks = raw.into_checks();
        let names = |suites: &[CheckSuite]| -> Vec<(String, Option<u32>)> {
            suites
                .iter()
                .map(|s| (s.name.clone(), s.run.as_ref().map(|r| r.number)))
                .collect()
        };
        assert_eq!(
            names(&checks.suites),
            [
                ("Aikido".into(), None),
                ("Validate PR".into(), Some(3402)),
                ("Build".into(), Some(2799)),
            ]
        );
        assert_eq!(checks.suites[0].checks[0].name, "Secrets");
        assert_eq!(
            names(&checks.others),
            [("Deploy".into(), Some(1052)), ("Deploy".into(), Some(1051))]
        );
        assert_eq!(checks.statuses[0].description, None);
    }

    #[test]
    fn validates_repo_names() {
        assert_eq!(split_repo("acme/shop").unwrap(), ("acme", "shop"));
        assert_eq!(split_repo("a-b/c.d_e").unwrap(), ("a-b", "c.d_e"));
        for bad in ["acme", "acme/", "../etc", "a/..", "a/b/c", "a/b?x=1"] {
            assert!(split_repo(bad).is_err(), "{bad} should be rejected");
        }
    }

    fn search_result() -> serde_json::Value {
        json!({
            "number": 7, "title": "t", "url": "u", "isDraft": false,
            "updatedAt": "2026-10-01T00:00:00Z", "headRefName": "h", "baseRefName": "main",
            "headRefOid": "abc", "isCrossRepository": true,
            "additions": 1, "deletions": 2, "changedFiles": 1, "reviewDecision": "APPROVED",
            "author": { "login": "me", "avatarUrl": "a" },
            "repository": { "nameWithOwner": "o/r" },
            "assignees": { "nodes": [{ "login": "me" }] },
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "ERROR" } } }] },
            "comments": { "nodes": [] },
            "reviews": { "nodes": [] }
        })
    }

    #[test]
    fn counts_comments_and_reviews_as_activity() {
        let updated_at = |comments: serde_json::Value, reviews: serde_json::Value| {
            let mut raw = search_result();
            raw["comments"]["nodes"] = comments;
            raw["reviews"]["nodes"] = reviews;
            let raw: RawPull = serde_json::from_value(raw).unwrap();
            raw.into_summary("me", &HashSet::new()).updated_at
        };
        assert_eq!(updated_at(json!([]), json!([])), "2026-10-01T00:00:00Z");
        assert_eq!(
            updated_at(
                json!([{ "at": "2026-10-03T00:00:00Z" }]),
                json!([{ "at": "2026-10-04T00:00:00Z" }, { "at": "2026-10-02T00:00:00Z" }]),
            ),
            "2026-10-04T00:00:00Z"
        );
        // A pending review is no activity yet, and older comments never win.
        assert_eq!(
            updated_at(
                json!([{ "at": "2026-09-01T00:00:00Z" }]),
                json!([{ "at": null }])
            ),
            "2026-10-01T00:00:00Z"
        );
    }

    #[test]
    fn derives_roles_from_search_results() {
        let raw: RawPull = serde_json::from_value(search_result()).unwrap();
        let requested = [("o/r".to_string(), 7)].into_iter().collect();
        let pr = raw.into_summary("me", &requested);
        assert_eq!(
            pr.roles,
            [Role::Author, Role::ReviewRequested, Role::Assignee]
        );
        assert_eq!(pr.checks, Some(CheckState::Failure));
        assert_eq!(pr.review_decision, Some(ReviewDecision::Approved));
        assert!(pr.is_fork);
        assert_eq!(pr.head_sha, "abc");
    }

    fn new_comment(start: (Side, u32), end: (Side, u32)) -> NewComment {
        let anchor = |(side, line)| shared::Anchor { side, line };
        NewComment {
            commit: "abc".into(),
            path: "lib/greeter.rb".into(),
            start: anchor(start),
            end: anchor(end),
            body: "Why?".into(),
        }
    }

    #[test]
    fn posts_comments_on_a_line_or_a_range() {
        let created = ("201 Created", "", "{}");
        let server = serve(vec![created; 2]);
        let github = client(&server);
        let line = new_comment((Side::Old, 12), (Side::Old, 12));
        block_on(github.add_comment("o/r", 7, &line)).unwrap();
        let range = new_comment((Side::Old, 3), (Side::New, 4));
        block_on(github.add_comment("o/r", 7, &range)).unwrap();

        let (request_line, body) = request(&server, 0);
        assert_eq!(request_line, "POST /repos/o/r/pulls/7/comments HTTP/1.1");
        assert_eq!(
            body,
            json!({ "body": "Why?", "commit_id": "abc", "path": "lib/greeter.rb",
                    "line": 12, "side": "LEFT" })
        );
        assert_eq!(
            request(&server, 1).1,
            json!({ "body": "Why?", "commit_id": "abc", "path": "lib/greeter.rb",
                    "line": 4, "side": "RIGHT", "start_line": 3, "start_side": "LEFT" })
        );
    }

    #[test]
    fn replies_to_a_threads_first_comment() {
        let server = serve(vec![("201 Created", "", "{}")]);
        block_on(client(&server).reply("o/r", 7, 4184934463, "Done")).unwrap();
        let (request_line, body) = request(&server, 0);
        assert_eq!(
            request_line,
            "POST /repos/o/r/pulls/7/comments/4184934463/replies HTTP/1.1"
        );
        assert_eq!(body, json!({ "body": "Done" }));
    }

    #[test]
    fn never_repeats_a_comment() {
        let server = serve(vec![("502 Bad Gateway", "", ""), ("201 Created", "", "{}")]);
        let comment = new_comment((Side::New, 4), (Side::New, 4));
        assert!(block_on(client(&server).add_comment("o/r", 7, &comment)).is_err());
        assert_eq!(*server.requests.lock().unwrap(), 1);
    }

    #[test]
    fn explains_a_pending_review() {
        // What GitHub answers while you have a review pending.
        let pending = (
            "422 Unprocessable Entity",
            "",
            r#"{"message":"Validation Failed","errors":[{"resource":"PullRequestReview","code":"custom","field":"user_id","message":"user_id can only have one pending review per pull request"}]}"#,
        );
        let server = serve(vec![pending; 2]);
        let github = client(&server);
        let comment = new_comment((Side::New, 4), (Side::New, 4));
        let errors = [
            block_on(github.add_comment("o/r", 7, &comment)).unwrap_err(),
            block_on(github.reply("o/r", 7, 1, "Done")).unwrap_err(),
        ];
        for error in errors {
            assert!(
                error.to_string().starts_with("You have a review pending"),
                "{error}"
            );
        }
    }

    #[test]
    fn resolves_and_unresolves_threads() {
        let server = serve(vec![
            (
                "200 OK",
                "",
                r#"{"data":{"resolveReviewThread":{"thread":{"isResolved":true}}}}"#,
            ),
            (
                "200 OK",
                "",
                r#"{"data":{"unresolveReviewThread":null},"errors":[{"message":"Could not resolve to a node with the global id of 'x'"}]}"#,
            ),
        ]);
        let github = client(&server);
        block_on(github.set_resolved("PRRT_1", true)).unwrap();
        let error = block_on(github.set_resolved("x", false)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Could not resolve to a node with the global id of 'x'"
        );

        let (request_line, body) = request(&server, 0);
        assert_eq!(request_line, "POST /graphql HTTP/1.1");
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("resolveReviewThread")
        );
        assert_eq!(body["variables"], json!({ "thread": "PRRT_1" }));
        let query = request(&server, 1).1["query"].as_str().unwrap().to_string();
        assert!(query.contains("unresolveReviewThread"), "{query}");
    }

    #[test]
    fn reads_what_threads_allow() {
        let thread = |id: &str, line: u32, start: (u32, Option<&str>), subject: &str| {
            json!({
                "id": id, "isResolved": false, "isOutdated": false, "path": "a.rb",
                "line": line, "startLine": start.0, "diffSide": "RIGHT",
                "startDiffSide": start.1, "subjectType": subject,
                "viewerCanReply": true, "viewerCanResolve": true, "viewerCanUnresolve": false,
                "comments": { "nodes": [{
                    "databaseId": 41, "author": null, "bodyHTML": "<p>x</p>",
                    "createdAt": "2026-10-05T10:00:00Z", "url": "u"
                }] }
            })
        };
        let raw: RawDetail = serde_json::from_value(json!({
            "bodyHTML": "", "createdAt": "2026-10-05T09:00:00Z", "url": "u",
            "headRefOid": "abc", "author": null,
            "comments": { "nodes": [] },
            "reviews": { "nodes": [] },
            "reviewThreads": { "nodes": [
                thread("single", 4, (4, None), "LINE"),
                thread("range", 4, (3, Some("LEFT")), "LINE"),
                thread("file", 1, (1, None), "FILE"),
            ] }
        }))
        .unwrap();
        let threads: Vec<ReviewThread> = raw
            .into_detail()
            .timeline
            .into_iter()
            .filter_map(|item| match item {
                TimelineItem::Thread(thread) => Some(thread),
                _ => None,
            })
            .collect();
        let lines: Vec<_> = threads
            .iter()
            .map(|t| (t.id.as_str(), t.start_line, t.line))
            .collect();
        assert_eq!(
            lines,
            [
                ("single", None, Some(4)),
                ("range", Some(3), Some(4)),
                ("file", None, None),
            ]
        );
        assert_eq!(threads[0].root_comment_id, Some(41));
        assert!(threads[0].can_reply && threads[0].can_resolve && !threads[0].can_unresolve);
    }
}
