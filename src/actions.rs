//! The Checks pane: the workflow runs and other checks on the pull request's
//! head, and the state behind the Logs tab a job opens in.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use shared::actions::parse_log;
use shared::{CheckRun, CheckState, CheckSuite, Checks, Job, RunState, StatusCheck, StepLog};

use crate::api;
use crate::app::{Ctx, Load, SideTab, Tab};
use crate::code_view::load_latest;
use crate::util;

/// How often what is still running is looked at again, while it's on screen.
const POLL_EVERY: Duration = Duration::from_secs(15);

/// A pull request's checks, with the PR they belong to.
#[derive(Debug, PartialEq)]
pub struct LoadedChecks {
    pub repo: String,
    pub number: u32,
    pub checks: Checks,
}

/// The workflow run the Logs tab shows, and the job asked for, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct LogTarget {
    pub repo: String,
    pub run: u64,
    pub job: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct LoadedRun {
    pub repo: String,
    pub run: u64,
    /// Every attempt's jobs.
    pub jobs: Vec<Job>,
}

impl LoadedRun {
    /// The jobs of the attempt on show: the asked-for job's, else the latest.
    pub fn attempt(&self, job: Option<u64>) -> Vec<&Job> {
        let attempt = job
            .and_then(|id| self.jobs.iter().find(|j| j.id == id))
            .or_else(|| self.jobs.iter().max_by_key(|j| j.run_attempt))
            .map(|j| j.run_attempt);
        self.jobs
            .iter()
            .filter(|j| Some(j.run_attempt) == attempt)
            .collect()
    }

    /// The job asked for, else the first that failed, else the first.
    fn pick(&self, job: Option<u64>) -> Option<&Job> {
        let jobs = self.attempt(job);
        job.and_then(|id| jobs.iter().find(|j| j.id == id))
            .or_else(|| jobs.iter().find(|j| j.state == RunState::Failure))
            .or_else(|| jobs.first())
            .copied()
    }
}

/// A finished job's log, split into its steps.
#[derive(Debug, PartialEq)]
pub struct JobLog {
    pub job: u64,
    pub steps: Vec<StepLog>,
}

#[derive(Clone, Copy)]
pub struct ActionsState {
    pub checks: RwSignal<Load<Arc<LoadedChecks>>>,
    pub refreshing: RwSignal<bool>,
    pub target: RwSignal<Option<LogTarget>>,
    pub run: RwSignal<Load<Arc<LoadedRun>>>,
    /// The job the Logs tab shows.
    pub job: Memo<Option<Arc<Job>>>,
    pub log: RwSignal<Load<Arc<JobLog>>>,
    /// The Logs tab's unfolded steps, by number.
    pub expanded: RwSignal<HashSet<u32>>,
    pub query: RwSignal<String>,
    /// The search hit ↵ went to last.
    pub hit: RwSignal<usize>,
    checks_epoch: RwSignal<u32>,
    run_epoch: RwSignal<u32>,
}

impl Default for ActionsState {
    fn default() -> Self {
        let target = RwSignal::new(None::<LogTarget>);
        let run = RwSignal::new(Load::<Arc<LoadedRun>>::Idle);
        let job = Memo::new(move |_| {
            let (run_id, wanted) = target.with(|t| t.as_ref().map(|t| (t.run, t.job)))?;
            run.with(|run| match run {
                Load::Ready(run) if run.run == run_id => run.pick(wanted).cloned().map(Arc::new),
                _ => None,
            })
        });
        ActionsState {
            checks: RwSignal::new(Load::Idle),
            refreshing: RwSignal::new(false),
            target,
            run,
            job,
            log: RwSignal::new(Load::Idle),
            expanded: RwSignal::new(HashSet::new()),
            query: RwSignal::new(String::new()),
            hit: RwSignal::new(0),
            checks_epoch: RwSignal::new(0),
            run_epoch: RwSignal::new(0),
        }
    }
}

impl Ctx {
    /// Shows a job's log in the Logs tab; without a job, the run's first
    /// failed one.
    pub fn open_job(&self, run: u64, job: Option<u64>) {
        let Some(repo) = self
            .selected
            .with_untracked(|pr| pr.as_ref().map(|pr| pr.repo.clone()))
        else {
            return;
        };
        let actions = self.actions;
        // What was loaded before may have finished since.
        actions.run_epoch.update(|e| *e += 1);
        actions.target.set(Some(LogTarget { repo, run, job }));
        self.tab.set(Tab::Logs);
    }

    pub fn refresh_checks(&self) {
        self.actions.checks_epoch.update(|e| *e += 1);
    }

    /// The selected PR's checks at a glance: what's loaded, else what the
    /// inbox said.
    pub fn checks_rollup(&self) -> Option<RunState> {
        let pr = self.selected.get()?;
        let loaded = self.actions.checks.with(|checks| match checks {
            Load::Ready(c) if c.repo == pr.repo && c.number == pr.number => Some(rollup(&c.checks)),
            _ => None,
        });
        loaded.unwrap_or_else(|| {
            pr.checks.map(|state| match state {
                CheckState::Success => RunState::Success,
                CheckState::Failure => RunState::Failure,
                CheckState::Pending => RunState::InProgress,
            })
        })
    }
}

/// The states of the PR's own checks; other runs on the commit don't count.
fn states(checks: &Checks) -> impl Iterator<Item = RunState> + '_ {
    checks
        .suites
        .iter()
        .flat_map(|s| s.checks.iter().map(|c| c.state))
        .chain(checks.statuses.iter().map(|s| s.state))
}

/// Something on the commit, a deployment included, is still going.
fn running(checks: &Checks) -> bool {
    let others = checks
        .others
        .iter()
        .flat_map(|s| s.checks.iter().map(|c| c.state));
    states(checks).chain(others).any(|s| !s.is_done())
}

fn rollup(checks: &Checks) -> Option<RunState> {
    let states: Vec<RunState> = states(checks).collect();
    if states.contains(&RunState::Failure) {
        Some(RunState::Failure)
    } else if states.iter().any(|s| !s.is_done()) {
        Some(RunState::InProgress)
    } else if states.contains(&RunState::Success) {
        Some(RunState::Success)
    } else {
        None
    }
}

/// Loads the selected PR's checks while the pane shows them, the run and
/// log the Logs tab shows, and keeps what's still running up to date.
pub fn setup(ctx: Ctx, key: Memo<Option<(String, u32)>>) {
    let state = ctx.actions;

    // Another pull request's logs make no sense next to this one's diff.
    Effect::new(move |previous: Option<Option<(String, u32)>>| {
        let current = key.get();
        if previous.is_some_and(|previous| previous != current) {
            state.target.set(None);
            state.expanded.set(HashSet::new());
            state.hit.set(0);
            if ctx.tab.get_untracked() == Tab::Logs {
                ctx.tab.set(Tab::Diff);
            }
        }
        current
    });

    // Checks load once the pane shows them, stay while it shows the
    // conversation, and refresh when it comes back.
    let showing_checks = Memo::new(move |_| {
        ctx.layout.conversation.open.get() && ctx.side_tab.get() == SideTab::Checks
    });
    Effect::new(move |previous: Option<bool>| {
        let now = showing_checks.get();
        if now && previous == Some(false) {
            ctx.refresh_checks();
        }
        now
    });
    let checks_key = Memo::new(move |previous: Option<&Option<(String, u32, u32)>>| {
        let pr = key.get();
        if showing_checks.get() {
            let epoch = state.checks_epoch.get();
            return pr.map(|(repo, number)| (repo, number, epoch));
        }
        previous
            .cloned()
            .flatten()
            .filter(|(repo, number, _)| pr.as_ref().is_some_and(|(r, n)| r == repo && n == number))
    });
    load_latest(
        checks_key,
        state.checks,
        |loaded, (repo, number, _)| loaded.repo == *repo && loaded.number == *number,
        move |(repo, number, _)| {
            state.refreshing.set(true);
            async move {
                let checks = api::pull_checks(&repo, number).await;
                state.refreshing.set(false);
                checks.map(|checks| {
                    Arc::new(LoadedChecks {
                        repo,
                        number,
                        checks,
                    })
                })
            }
        },
    );

    let run_key = Memo::new(move |_| {
        let target = state.target.get()?;
        Some((target.repo, target.run, state.run_epoch.get()))
    });
    load_latest(
        run_key,
        state.run,
        |loaded, (repo, run, _)| loaded.repo == *repo && loaded.run == *run,
        |(repo, run, _)| async move {
            let jobs = api::run_jobs(&repo, run).await?;
            Ok(Arc::new(LoadedRun { repo, run, jobs }))
        },
    );

    // GitHub only hands out a job's log once the job is done.
    let log_key = Memo::new(move |_| {
        let job = state.job.get()?;
        let repo = state.target.with(|t| t.as_ref().map(|t| t.repo.clone()))?;
        job.state
            .is_done()
            .then(|| (repo, job.id, job.steps.clone()))
    });
    load_latest(
        log_key,
        state.log,
        |log, (_, job, _)| log.job == *job,
        |(repo, job, steps)| async move {
            let raw = api::job_log(&repo, job).await?;
            Ok(Arc::new(JobLog {
                job,
                steps: parse_log(&raw, &steps),
            }))
        },
    );

    let run_running = move || {
        state.run.with_untracked(
            |run| matches!(run, Load::Ready(run) if run.jobs.iter().any(|j| !j.state.is_done())),
        )
    };
    Effect::new(move |previous: Option<Tab>| {
        let tab = ctx.tab.get();
        if tab == Tab::Logs && previous.is_some_and(|p| p != Tab::Logs) && run_running() {
            state.run_epoch.update(|e| *e += 1);
        }
        tab
    });
    let poll = set_interval_with_handle(
        move || {
            if ctx.tab.get_untracked() == Tab::Logs && run_running() {
                state.run_epoch.update(|e| *e += 1);
            }
            let checks_running = state
                .checks
                .with_untracked(|checks| matches!(checks, Load::Ready(c) if running(&c.checks)));
            if showing_checks.get_untracked() && checks_running {
                ctx.refresh_checks();
            }
        },
        POLL_EVERY,
    )
    .ok();
    on_cleanup(move || {
        if let Some(poll) = poll {
            poll.clear();
        }
    });
}

/// A check's state as GitHub draws it, in glyphs.
pub fn state_glyph(state: RunState) -> impl IntoView {
    let (class, glyph) = match state {
        RunState::Success => ("state success", "✓"),
        RunState::Failure => ("state failure", "✕"),
        RunState::InProgress => ("state running", "●"),
        RunState::Queued => ("state queued", "○"),
        RunState::Cancelled => ("state muted", "⊘"),
        RunState::Skipped | RunState::Neutral => ("state muted", "−"),
    };
    view! { <span class=class title=state_word(state)>{glyph}</span> }
}

pub fn state_word(state: RunState) -> &'static str {
    match state {
        RunState::Success => "Succeeded",
        RunState::Failure => "Failed",
        RunState::InProgress => "In progress",
        RunState::Queued => "Queued",
        RunState::Cancelled => "Cancelled",
        RunState::Skipped => "Skipped",
        RunState::Neutral => "Neutral",
    }
}

/// How long it took, or how it stands when that says more.
pub fn time_label(state: RunState, started: Option<&str>, completed: Option<&str>) -> String {
    match state {
        RunState::Queued => "queued".into(),
        RunState::Skipped => "skipped".into(),
        RunState::InProgress => util::duration(started, None).unwrap_or_else(|| "running".into()),
        _ => util::duration(started, completed).unwrap_or_default(),
    }
}

#[component]
pub fn ChecksList() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.actions;
    move || match state.checks.get() {
        Load::Idle => ().into_any(),
        Load::Loading => view! { <p class="loading">"Loading checks…"</p> }.into_any(),
        Load::Failed(error) => view! { <div class="error-banner">{error}</div> }.into_any(),
        Load::Ready(loaded) => checks_view(ctx, &loaded.checks),
    }
}

fn checks_view(ctx: Ctx, checks: &Checks) -> AnyView {
    if checks.suites.is_empty() && checks.statuses.is_empty() && checks.others.is_empty() {
        return view! { <p class="empty">"No checks on this commit."</p> }.into_any();
    }
    let suites = checks
        .suites
        .iter()
        .map(|suite| suite_view(ctx, suite))
        .collect_view();
    let statuses = (!checks.statuses.is_empty()).then(|| {
        let rows = checks.statuses.iter().map(status_row).collect_view();
        view! {
            <section class="suite">
                <header class="suite-head">
                    <span class="suite-name">"Statuses"</span>
                </header>
                <ul class="suite-checks">{rows}</ul>
            </section>
        }
    });
    let others = (!checks.others.is_empty()).then(|| {
        let runs = checks
            .others
            .iter()
            .map(|suite| suite_view(ctx, suite))
            .collect_view();
        view! {
            <h3
                class="checks-section"
                title="Runs on this commit that GitHub doesn't count as the PR's checks, such as deployments started by hand"
            >
                "Other runs on this commit"
            </h3>
            {runs}
        }
    });
    view! {
        <p class="checks-summary">{summary(checks)}</p>
        {suites}
        {statuses}
        {others}
    }
    .into_any()
}

/// "1 failing · 2 running · 14 passed".
fn summary(checks: &Checks) -> String {
    let mut counts = [0usize; 5];
    for state in states(checks) {
        let slot = match state {
            RunState::Failure => 0,
            RunState::InProgress | RunState::Queued => 1,
            RunState::Cancelled => 2,
            RunState::Success => 3,
            RunState::Skipped | RunState::Neutral => 4,
        };
        counts[slot] += 1;
    }
    let parts: Vec<String> = ["failing", "running", "cancelled", "passed", "skipped"]
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|(word, count)| format!("{count} {word}"))
        .collect();
    if parts.is_empty() {
        return "No checks on this PR".into();
    }
    parts.join(" · ")
}

fn suite_view(ctx: Ctx, suite: &CheckSuite) -> impl IntoView + use<> {
    let run = suite.run.as_ref().map(|run| run.id);
    let meta = suite
        .run
        .as_ref()
        .map(|run| format!("#{} · {}", run.number, run.event.replace('_', " ")));
    let link = suite.run.as_ref().map(|run| {
        view! {
            <a class="suite-link" href=run.url.clone() title="Open the run on GitHub">
                "↗"
            </a>
        }
    });
    let rows = suite
        .checks
        .iter()
        .map(|check| check_row(ctx, check, run))
        .collect_view();
    view! {
        <section class="suite">
            <header class="suite-head">
                <span class="suite-name" title=suite.name.clone()>{suite.name.clone()}</span>
                <span class="suite-meta">{meta}</span>
                {link}
            </header>
            <ul class="suite-checks">{rows}</ul>
        </section>
    }
}

/// An Actions job opens its log here; any other check opens its own site.
fn check_row(ctx: Ctx, check: &CheckRun, run: Option<u64>) -> impl IntoView + use<> {
    let id = check.id;
    let url = check.url.clone();
    let open = move |_| match (run, &url) {
        (Some(run), _) => ctx.open_job(run, Some(id)),
        (None, Some(url)) => util::open_external(url.clone()),
        (None, None) => {}
    };
    let selected = move || {
        run.is_some()
            && ctx.tab.get() == Tab::Logs
            && ctx
                .actions
                .job
                .with(|job| job.as_ref().is_some_and(|job| job.id == id))
    };
    let title = if run.is_some() {
        "Show this job's log"
    } else {
        "Open this check's details in the browser"
    };
    let time = time_label(
        check.state,
        check.started_at.as_deref(),
        check.completed_at.as_deref(),
    );
    view! {
        <li>
            <button class="check-row" class:selected=selected title=title on:click=open>
                {state_glyph(check.state)}
                <span class="name">{check.name.clone()}</span>
                <span class="time">{time}</span>
                {run.is_none().then(|| view! { <span class="external">"↗"</span> })}
            </button>
        </li>
    }
}

fn status_row(status: &StatusCheck) -> impl IntoView + use<> {
    let url = status.url.clone();
    let open = move |_| {
        if let Some(url) = url.clone() {
            util::open_external(url);
        }
    };
    let description = status.description.clone().unwrap_or_default();
    let title = description.clone();
    view! {
        <li>
            <button
                class="check-row"
                title=title
                disabled=status.url.is_none()
                on:click=open
            >
                {state_glyph(status.state)}
                <span class="name">{status.context.clone()}</span>
                <span class="time">{description}</span>
            </button>
        </li>
    }
}
