//! The Logs tab: a GitHub Actions job's log the way GitHub shows it, as
//! collapsible steps and groups with terminal colours, and searchable.

use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use leptos::ev::KeyboardEvent;
use leptos::prelude::*;
use shared::actions::{Color, LogKind, LogLine, Style};
use shared::{Job, JobStep, RunState};

use crate::actions::{JobLog, state_glyph, state_word, time_label};
use crate::app::{Ctx, Load, SideTab};
use crate::code_view::find_all;
use crate::util::{self, Icon};

/// Typing pauses this long before a search runs.
const SEARCH_DELAY: Duration = Duration::from_millis(150);
/// Hits beyond this aren't marked; a one-letter search would mark everything.
const MAX_HITS: usize = 5000;
/// Top-level lines per block. Blocks off screen skip layout and painting,
/// which keeps a step of ten thousand lines scrolling smoothly.
const BLOCK_LINES: usize = 250;

/// A search hit: a step (by index in the log), a line in it, and the byte
/// range on that line.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hit {
    step: usize,
    line: u32,
    start: u32,
    end: u32,
}

#[derive(Debug, Default, PartialEq)]
struct Hits {
    all: Vec<Hit>,
    capped: bool,
}

impl Hits {
    /// A step's hits as `(line, start, end, k)`, `k` being the hit's place
    /// among all of them.
    fn in_step(&self, step: usize) -> Vec<(u32, u32, u32, usize)> {
        self.all
            .iter()
            .enumerate()
            .filter(|(_, hit)| hit.step == step)
            .map(|(k, hit)| (hit.line, hit.start, hit.end, k))
            .collect()
    }
}

fn search(log: &JobLog, query: &str) -> Hits {
    let mut all = Vec::new();
    if query.is_empty() {
        return Hits::default();
    }
    for (step, lines) in log.steps.iter().enumerate() {
        let found = find_all(lines.lines.iter().map(|l| l.text.as_str()), query, false);
        for (line, start, end) in found {
            if all.len() == MAX_HITS {
                return Hits { all, capped: true };
            }
            all.push(Hit {
                step,
                line,
                start,
                end,
            });
        }
    }
    Hits { all, capped: false }
}

#[component]
pub fn LogPane() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.actions;

    let applied = RwSignal::new(String::new());
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        let query = state.query.get();
        generation.update_value(|g| *g += 1);
        let current = generation.get_value();
        set_timeout(
            move || {
                if generation.get_value() == current {
                    applied.set(query.trim().to_string());
                    state.hit.set(0);
                }
            },
            SEARCH_DELAY,
        );
    });
    let hits = Memo::new(move |_| {
        let query = applied.get();
        state.log.with(|log| match log {
            Load::Ready(log) => Arc::new(search(log, &query)),
            _ => Arc::new(Hits::default()),
        })
    });
    // Steps with hits unfold, as on GitHub.
    Effect::new(move |_| {
        let numbers: Vec<u32> = hits.with(|hits| {
            state.log.with_untracked(|log| match log {
                Load::Ready(log) => hits
                    .all
                    .iter()
                    .map(|hit| log.steps[hit.step].number)
                    .collect(),
                _ => Vec::new(),
            })
        });
        if !numbers.is_empty() {
            state.expanded.update(|e| e.extend(numbers));
        }
    });
    Effect::new(move |_| {
        let total = hits.with(|h| h.all.len());
        let index = state.hit.get();
        if total > 0 {
            let k = index.min(total - 1);
            set_timeout(move || show_hit(k), Duration::from_millis(30));
        }
    });

    // A newly opened job starts at the top, with its failed steps unfolded
    // and the first error in view.
    Effect::new(move |previous: Option<Option<u64>>| {
        let loaded = state.log.with(|log| match log {
            Load::Ready(log) => Some((log.job, failed_steps(ctx, log))),
            _ => None,
        });
        let job = loaded.as_ref().map(|(job, _)| *job);
        if let Some((job, failed)) = loaded
            && previous != Some(Some(job))
        {
            util::scroll_top("log-scroll");
            let any_failed = !failed.is_empty();
            state.expanded.set(failed);
            if any_failed {
                set_timeout(reveal_error, Duration::from_millis(50));
            }
        }
        job
    });

    // The tab is always there; until a job is opened, it says where to find one.
    let no_job = move || state.target.with(Option::is_none);
    let checks_hidden =
        move || !ctx.layout.conversation.open.get() || ctx.side_tab.get() != SideTab::Checks;
    let show_checks = move |_| {
        ctx.side_tab.set(SideTab::Checks);
        ctx.layout.conversation.open.set(true);
    };

    view! {
        <section class="logs">
            <div class="empty-state" class:hidden=move || !no_job()>
                <p>
                    "Pick a job in Checks to read its log here."
                    <button class="link" class:hidden=move || !checks_hidden() on:click=show_checks>
                        "Show checks"
                    </button>
                </p>
            </div>
            <LogHeader hits/>
            <div class="log-scroll" id="log-scroll" class:hidden=no_job>
                <LogBody hits/>
            </div>
        </section>
    }
}

/// The numbers of a job's failed steps that have output.
fn failed_steps(ctx: Ctx, log: &JobLog) -> HashSet<u32> {
    let job = ctx.actions.job.get_untracked();
    let failed: HashSet<u32> = job
        .iter()
        .flat_map(|job| job.steps.iter())
        .filter(|step| step.state == RunState::Failure)
        .map(|step| step.number)
        .collect();
    log.steps
        .iter()
        .filter(|step| !step.lines.is_empty() && failed.contains(&step.number))
        .map(|step| step.number)
        .collect()
}

fn reveal_error() {
    let first = util::query_all("#log-scroll .k-error")
        .into_iter()
        .next()
        .or_else(|| {
            util::query_all("#log-scroll .step.failed")
                .into_iter()
                .next()
        });
    if let Some(element) = first {
        let options = web_sys::ScrollIntoViewOptions::new();
        options.set_block(web_sys::ScrollLogicalPosition::Center);
        element.scroll_into_view_with_scroll_into_view_options(&options);
    }
}

/// Marks hit `k` as the current one, unfolds the groups around it and
/// scrolls to it. Done in the DOM, so stepping through hits doesn't rebuild
/// the log.
fn show_hit(k: usize) {
    for mark in util::query_all("#log-scroll mark.hit.current") {
        let _ = mark.class_list().remove_1("current");
    }
    let pieces = util::query_all(&format!("#log-scroll mark[data-k=\"{k}\"]"));
    for piece in &pieces {
        let _ = piece.class_list().add_1("current");
    }
    let Some(first) = pieces.first() else {
        return;
    };
    let mut folded = first.closest("details:not([open])").ok().flatten();
    while let Some(details) = folded {
        let _ = details.set_attribute("open", "");
        folded = details.closest("details:not([open])").ok().flatten();
    }
    util::scroll_center(&format!("log-hit-{k}"));
}

#[component]
fn LogHeader(hits: Memo<Arc<Hits>>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.actions;
    let step = move |forward: bool| {
        let count = hits.with_untracked(|h| h.all.len());
        if count == 0 {
            return;
        }
        state.hit.update(|i| {
            *i = if forward {
                (*i + 1) % count
            } else {
                (*i + count - 1) % count
            }
        });
    };
    let on_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            ev.prevent_default();
            step(!ev.shift_key());
        }
        "Escape" => {
            state.query.set(String::new());
            util::blur("log-search");
        }
        _ => {}
    };
    let count = move || {
        let (total, capped) = hits.with(|h| (h.all.len(), h.capped));
        if state.query.with(|q| q.trim().is_empty()) {
            String::new()
        } else if total == 0 {
            "No results".into()
        } else {
            let more = if capped { "+" } else { "" };
            format!("{}/{total}{more}", state.hit.get().min(total - 1) + 1)
        }
    };
    // Without hits there's nothing to step through.
    let no_hits = move || hits.with(|h| h.all.is_empty());
    let any_open = move || state.expanded.with(|e| !e.is_empty());
    let fold_all = move |_| {
        if any_open() {
            state.expanded.set(HashSet::new());
            return;
        }
        let numbers = state.log.with_untracked(|log| match log {
            Load::Ready(log) => log
                .steps
                .iter()
                .filter(|s| !s.lines.is_empty())
                .map(|s| s.number)
                .collect(),
            _ => HashSet::new(),
        });
        state.expanded.set(numbers);
    };
    // The Checks pane already lists the job and how it went, so the header
    // only names it; the rest is in the tooltip. Only these parts follow the
    // job, so polling a running one never steals the search box's focus.
    let title = move || {
        state.job.get().map(|job| {
            let tooltip = format!("{} / {}: {}", job.workflow, job.name, job_summary(&job));
            view! {
                <span class="log-title" title=tooltip>
                    {state_glyph(job.state)}
                    <span class="log-job">{job.name.clone()}</span>
                    <span class="log-meta">{job.workflow.clone()}</span>
                </span>
            }
        })
    };
    let link = move || {
        state.job.with(|job| {
            job.as_ref().map(|job| {
                view! {
                    <a class="btn small icon" href=job.url.clone() title="Open this job on GitHub">
                        <Icon path=util::GITHUB_ICON size=14/>
                    </a>
                }
            })
        })
    };
    view! {
        <header class="log-head" class:hidden=move || state.target.with(Option::is_none)>
            {title}
            <JobPicker/>
            <input
                id="log-search"
                class="search"
                type="text"
                spellcheck="false"
                autocomplete="off"
                placeholder="Search logs"
                prop:value=move || state.query.get()
                on:input=move |ev| state.query.set(event_target_value(&ev))
                on:keydown=on_key
            />
            <span class="find-count">{count}</span>
            <button
                class="opt"
                class:hidden=no_hits
                title="Previous hit (⇧↵)"
                on:click=move |_| step(false)
            >
                "↑"
            </button>
            <button
                class="opt"
                class:hidden=no_hits
                title="Next hit (↵)"
                on:click=move |_| step(true)
            >
                "↓"
            </button>
            <button
                class="btn small icon"
                title=move || if any_open() { "Collapse every step" } else { "Expand every step" }
                on:click=fold_all
            >
                {move || {
                    let path = if any_open() { util::FOLD_ICON } else { util::UNFOLD_ICON };
                    view! { <Icon path size=14/> }
                }}
            </button>
            {link}
        </header>
    }
}

/// "Failed 3h ago in 5m 16s", "In progress for 2m 3s", "Queued".
fn job_summary(job: &Job) -> String {
    let took = util::duration(job.started_at.as_deref(), job.completed_at.as_deref());
    match job.state {
        RunState::Queued => "Queued".into(),
        RunState::InProgress => match took {
            Some(took) => format!("In progress for {took}"),
            None => "In progress".into(),
        },
        state => {
            let mut summary = state_word(state).to_string();
            if let Some(at) = &job.completed_at {
                summary.push(' ');
                summary.push_str(&util::ago(at));
                summary.push_str(" ago");
            }
            if let Some(took) = took {
                summary.push_str(" in ");
                summary.push_str(&took);
            }
            summary
        }
    }
}

/// Switches between the run's jobs when the Checks pane can't: for a run
/// it doesn't list, such as an older one opened from a link in a comment.
#[component]
fn JobPicker() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.actions;
    move || {
        let target = state.target.get()?;
        let listed = state.checks.with(|checks| {
            matches!(checks, Load::Ready(loaded) if loaded
                .checks
                .suites
                .iter()
                .chain(&loaded.checks.others)
                .any(|suite| suite.run.as_ref().is_some_and(|run| run.id == target.run)))
        });
        if listed {
            return None;
        }
        let current = state.job.with(|job| job.as_ref().map(|job| job.id));
        state.run.with(|run| {
            let Load::Ready(run) = run else {
                return None;
            };
            let jobs = run.attempt(target.job.or(current));
            if jobs.len() < 2 {
                return None;
            }
            let run_id = run.run;
            let options = jobs
                .into_iter()
                .map(|job| {
                    view! {
                        <option value=job.id.to_string() selected=current == Some(job.id)>
                            {job.name.clone()}
                        </option>
                    }
                })
                .collect_view();
            let pick = move |ev| {
                if let Ok(id) = event_target_value(&ev).parse() {
                    ctx.open_job(run_id, Some(id));
                }
            };
            Some(view! {
                <select class="job-picker" title="Another job of this run" on:change=pick>
                    {options}
                </select>
            })
        })
    }
}

#[component]
fn LogBody(hits: Memo<Arc<Hits>>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.actions;
    move || {
        let Some(job) = state.job.get() else {
            return match state.run.get() {
                Load::Failed(error) => view! { <div class="error-banner">{error}</div> }.into_any(),
                Load::Ready(_) => view! { <p class="empty">"This run has no jobs."</p> }.into_any(),
                _ => view! { <p class="loading">"Loading jobs…"</p> }.into_any(),
            };
        };
        let (notice, log) = if !job.state.is_done() {
            let notice = view! {
                <p class="log-notice">
                    "GitHub hands out a job's log once the job has finished; until then, "
                    <a href=job.url.clone()>"follow it live on GitHub"</a>
                    ". The steps below keep up."
                </p>
            };
            (Some(notice.into_any()), None)
        } else {
            match state.log.get() {
                Load::Ready(log) if log.job == job.id => (None, Some(log)),
                Load::Failed(error) => (
                    Some(view! { <div class="error-banner">{error}</div> }.into_any()),
                    None,
                ),
                _ => (
                    Some(view! { <p class="loading">"Loading log…"</p> }.into_any()),
                    None,
                ),
            }
        };
        let steps = steps_view(&job, log, hits);
        view! {
            {notice}
            {steps}
        }
        .into_any()
    }
}

fn steps_view(job: &Job, log: Option<Arc<JobLog>>, hits: Memo<Arc<Hits>>) -> AnyView {
    // A log the steps' times couldn't split shows whole.
    if let Some(log) = log
        .as_ref()
        .filter(|log| log.steps.first().is_some_and(|s| s.number == 0))
    {
        let step = JobStep {
            number: 0,
            name: "Log".into(),
            state: job.state,
            started_at: job.started_at.clone(),
            completed_at: job.completed_at.clone(),
        };
        return view! { <Step step log=Some(log.clone()) index=Some(0) hits/> }.into_any();
    }
    job.steps
        .iter()
        .map(|step| {
            let index = log
                .as_ref()
                .and_then(|log| log.steps.iter().position(|s| s.number == step.number));
            view! { <Step step=step.clone() log=log.clone() index hits/> }
        })
        .collect_view()
        .into_any()
}

#[component]
fn Step(
    step: JobStep,
    log: Option<Arc<JobLog>>,
    /// Where the step's lines are in the log, if it printed any.
    index: Option<usize>,
    hits: Memo<Arc<Hits>>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = ctx.actions;
    let number = step.number;
    let has_output = log
        .as_ref()
        .zip(index)
        .is_some_and(|(log, i)| !log.steps[i].lines.is_empty());
    let open = Memo::new(move |_| has_output && state.expanded.with(|e| e.contains(&number)));
    let toggle = move |_| {
        state.expanded.update(|e| {
            if !e.remove(&number) {
                e.insert(number);
            }
        })
    };
    let step_hits = Memo::new(move |_| match index {
        Some(i) => hits.with(|h| h.in_step(i)),
        None => Vec::new(),
    });
    let html = move || match (&log, index) {
        (Some(log), Some(i)) => step_html(&log.steps[i].lines, &step_hits.get()),
        _ => String::new(),
    };
    let time = time_label(
        step.state,
        step.started_at.as_deref(),
        step.completed_at.as_deref(),
    );

    view! {
        <section class="step" class:failed=step.state == RunState::Failure>
            <button class="step-head" class:open=open disabled=!has_output on:click=toggle>
                <span class="chevron" class:open=open>{if has_output { "▸" } else { "" }}</span>
                {state_glyph(step.state)}
                <span class="step-name">{step.name}</span>
                <span class="step-time">{time}</span>
            </button>
            {move || open.get().then(|| view! { <div class="log" inner_html=html.clone()></div> })}
        </section>
    }
}

/// A step's lines as HTML: groups become `<details>`, open when they hold a
/// search hit. Built as one string, since a step can run to ten thousand
/// lines and the DOM parses those far faster than they could be created
/// one node at a time.
fn step_html(lines: &[LogLine], hits: &[(u32, u32, u32, usize)]) -> String {
    let hit_lines: HashSet<u32> = hits.iter().map(|h| h.0).collect();
    let mut open_groups = HashSet::new();
    let mut headers: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        headers.truncate(line.depth as usize);
        if hit_lines.contains(&(index as u32)) {
            open_groups.extend(headers.iter().copied());
        }
        if line.kind == LogKind::Group {
            headers.push(index);
        }
    }

    let mut html = String::with_capacity(lines.len() * 96);
    let mut depth = 0u8;
    let mut in_block = 0;
    let mut line_hits = hits.iter().peekable();
    html.push_str("<div class=\"block\">");
    for (index, line) in lines.iter().enumerate() {
        while depth > line.depth {
            html.push_str("</div></details>");
            depth -= 1;
        }
        if depth == 0 && in_block >= BLOCK_LINES {
            html.push_str("</div><div class=\"block\">");
            in_block = 0;
        }
        in_block += 1;
        let mut on_line = Vec::new();
        while let Some(hit) = line_hits.next_if(|h| h.0 as usize == index) {
            on_line.push((hit.1, hit.2, hit.3));
        }
        let classes = line_classes(line);
        if line.kind == LogKind::Group {
            let open = if open_groups.contains(&index) {
                " open"
            } else {
                ""
            };
            let _ = write!(
                html,
                "<details class=\"group\"{open}><summary class=\"{classes}\">"
            );
            line_html(&mut html, index, line, &on_line);
            html.push_str("</summary><div class=\"group-body\">");
            depth += 1;
        } else {
            let _ = write!(html, "<div class=\"{classes}\">");
            line_html(&mut html, index, line, &on_line);
            html.push_str("</div>");
        }
    }
    for _ in 0..depth {
        html.push_str("</div></details>");
    }
    html.push_str("</div>");
    html
}

fn line_classes(line: &LogLine) -> String {
    let kind = match line.kind {
        LogKind::Plain => "",
        LogKind::Group => " k-group",
        LogKind::Command => " k-command",
        LogKind::Error => " k-error",
        LogKind::Warning => " k-warning",
        LogKind::Notice => " k-notice",
        LogKind::Debug => " k-debug",
        LogKind::Section => " k-section",
    };
    match line.depth {
        0 => format!("line{kind}"),
        depth => format!("line{kind} d{}", depth.min(6)),
    }
}

/// A line's number and text, with its colours, links and search hits.
fn line_html(html: &mut String, index: usize, line: &LogLine, hits: &[(u32, u32, usize)]) {
    let _ = write!(
        html,
        "<span class=\"ln\">{}</span><span class=\"lt\">",
        index + 1
    );
    let text = &line.text;
    let links = links(text);
    if line.styles.is_empty() && hits.is_empty() && links.is_empty() {
        escape_into(html, text);
        html.push_str("</span>");
        return;
    }
    let mut cuts: Vec<u32> = line
        .styles
        .iter()
        .flat_map(|&(start, end, _)| [start, end])
        .chain(hits.iter().flat_map(|&(start, end, _)| [start, end]))
        .chain(links.iter().flat_map(|&(start, end)| [start, end]))
        .chain([0, text.len() as u32])
        .filter(|&cut| cut as usize <= text.len())
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let Some(piece) = text.get(start as usize..end as usize) else {
            continue;
        };
        let within = |from: u32, to: u32| from <= start && start < to;
        let link = links.iter().find(|&&(from, to)| within(from, to));
        let hit = hits.iter().find(|&&(from, to, _)| within(from, to));
        let style = line
            .styles
            .iter()
            .find(|&&(from, to, _)| within(from, to))
            .map(|(_, _, style)| style);
        if let Some(&(from, to)) = link {
            html.push_str("<a href=\"");
            escape_into(html, &text[from as usize..to as usize]);
            html.push_str("\">");
        }
        if let Some(&(from, _, k)) = hit {
            let id = if from == start {
                format!(" id=\"log-hit-{k}\"")
            } else {
                String::new()
            };
            let _ = write!(html, "<mark class=\"hit\" data-k=\"{k}\"{id}>");
        }
        match style {
            Some(style) => {
                html.push_str("<span");
                style_attributes(html, style);
                html.push('>');
                escape_into(html, piece);
                html.push_str("</span>");
            }
            None => escape_into(html, piece),
        }
        if hit.is_some() {
            html.push_str("</mark>");
        }
        if link.is_some() {
            html.push_str("</a>");
        }
    }
    html.push_str("</span>");
}

/// The theme's 16 colours are classes; the rest of the 256, and true
/// colours, are spelled out.
fn style_attributes(html: &mut String, style: &Style) {
    let mut classes = Vec::new();
    let mut inline = String::new();
    for (color, class, property) in [(style.fg, "fg", "color"), (style.bg, "bg", "background")] {
        match color {
            Some(Color::Index(n)) if n < 16 => classes.push(format!("{class}{n}")),
            Some(color) => {
                let (r, g, b) = rgb(color);
                let _ = write!(inline, "{property}:#{r:02x}{g:02x}{b:02x};");
            }
            None => {}
        }
    }
    for (on, class) in [
        (style.bold, "bold"),
        (style.dim, "dim"),
        (style.italic, "italic"),
        (style.underline, "underline"),
    ] {
        if on {
            classes.push(class.to_string());
        }
    }
    if !classes.is_empty() {
        let _ = write!(html, " class=\"{}\"", classes.join(" "));
    }
    if !inline.is_empty() {
        let _ = write!(html, " style=\"{inline}\"");
    }
}

/// xterm's 256-colour palette beyond the first 16.
fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Index(n @ 16..=231) => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            let n = n - 16;
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        Color::Index(n @ 232..=255) => {
            let v = 8 + (n - 232) * 10;
            (v, v, v)
        }
        Color::Index(_) => (128, 128, 128),
    }
}

/// Byte ranges of the web addresses in a line.
fn links(text: &str) -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("http") {
        let start = from + at;
        let rest = &text[start..];
        if !(rest.starts_with("https://") || rest.starts_with("http://")) {
            from = start + 4;
            continue;
        }
        let len = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(rest.len());
        let url = rest[..len].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '!', '?']);
        let end = start + url.len();
        if !url.ends_with("//") {
            found.push((start as u32, end as u32));
        }
        from = end.max(start + 4);
    }
    found
}

fn escape_into(html: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '"' => html.push_str("&quot;"),
            c => html.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::actions::parse_ansi;

    fn line(text: &str, kind: LogKind, depth: u8) -> LogLine {
        let (text, styles) = parse_ansi(text);
        LogLine {
            text,
            styles,
            kind,
            depth,
        }
    }

    #[test]
    fn escapes_and_colours_text() {
        let mut html = String::new();
        line_html(
            &mut html,
            0,
            &line(
                "\x1b[31m<b>\x1b[0m & see https://x.io/a?b=1.",
                LogKind::Plain,
                0,
            ),
            &[],
        );
        assert_eq!(
            html,
            "<span class=\"ln\">1</span><span class=\"lt\"><span class=\"fg1\">&lt;b&gt;</span> &amp; see \
             <a href=\"https://x.io/a?b=1\">https://x.io/a?b=1</a>.</span>"
        );
    }

    #[test]
    fn marks_hits_across_colours() {
        let mut html = String::new();
        line_html(
            &mut html,
            4,
            &line("ab\x1b[1mcd\x1b[0m", LogKind::Plain, 0),
            &[(1, 3, 7)],
        );
        assert!(html.contains("<mark class=\"hit\" data-k=\"7\" id=\"log-hit-7\">b</mark>"));
        assert!(
            html.contains("<mark class=\"hit\" data-k=\"7\"><span class=\"bold\">c</span></mark>")
        );
    }

    #[test]
    fn nests_groups_and_opens_those_with_hits() {
        let lines = [
            line("Run tests", LogKind::Group, 0),
            line("env", LogKind::Group, 1),
            line("CI=true", LogKind::Plain, 2),
            line("ok", LogKind::Plain, 0),
        ];
        let html = step_html(&lines, &[(2, 0, 2, 0)]);
        assert_eq!(html.matches("<details class=\"group\" open>").count(), 2);
        assert_eq!(html.matches("</details>").count(), 2);
        assert!(html.contains("class=\"line d2\""));
        assert!(html.ends_with("<span class=\"lt\">ok</span></div></div>"));
    }

    #[test]
    fn finds_links() {
        assert_eq!(
            links("see https://a.b/c, or http://d."),
            [(4, 17), (22, 30)]
        );
        assert_eq!(links("no http here, nor https://"), []);
    }

    #[test]
    fn converts_256_colours() {
        assert_eq!(rgb(Color::Index(196)), (255, 0, 0));
        assert_eq!(rgb(Color::Index(16)), (0, 0, 0));
        assert_eq!(rgb(Color::Index(244)), (128, 128, 128));
    }
}
