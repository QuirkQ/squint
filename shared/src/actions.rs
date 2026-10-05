//! The Checks pane's types, and the parser that turns a GitHub Actions job's
//! raw log into the steps and collapsible groups GitHub shows.

use serde::{Deserialize, Serialize};

/// Where a check, job or step stands: GitHub's status and conclusion in one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    InProgress,
    Success,
    /// Failed, timed out, or waiting for someone to act.
    Failure,
    Cancelled,
    Skipped,
    Neutral,
}

impl RunState {
    /// From a check's or job's status and conclusion, in the GraphQL API's
    /// spelling (`IN_PROGRESS`) or the REST API's (`in_progress`).
    pub fn from_github(status: &str, conclusion: Option<&str>) -> Self {
        match conclusion.map(str::to_ascii_lowercase).as_deref() {
            Some("success") => RunState::Success,
            Some("failure" | "timed_out" | "startup_failure" | "action_required") => {
                RunState::Failure
            }
            Some("cancelled") => RunState::Cancelled,
            Some("skipped") => RunState::Skipped,
            Some(_) => RunState::Neutral,
            None => match status.to_ascii_lowercase().as_str() {
                "in_progress" => RunState::InProgress,
                "completed" => RunState::Neutral,
                _ => RunState::Queued,
            },
        }
    }

    /// From a commit status, the older API some CI still reports through.
    pub fn from_status(state: &str) -> Self {
        match state.to_ascii_lowercase().as_str() {
            "success" => RunState::Success,
            "failure" | "error" => RunState::Failure,
            "pending" => RunState::InProgress,
            _ => RunState::Queued,
        }
    }

    pub fn is_done(self) -> bool {
        !matches!(self, RunState::Queued | RunState::InProgress)
    }

    /// Lists put what needs attention first: failures, then what's still
    /// running, then the rest.
    pub fn rank(self) -> u8 {
        match self {
            RunState::Failure => 0,
            RunState::InProgress => 1,
            RunState::Queued => 2,
            RunState::Cancelled => 3,
            RunState::Success => 4,
            RunState::Neutral => 5,
            RunState::Skipped => 6,
        }
    }
}

/// The checks on a pull request's head commit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Checks {
    /// The PR's checks, as GitHub counts them: workflow runs, latest per
    /// workflow and event, and other apps' check suites; failing first.
    pub suites: Vec<CheckSuite>,
    pub statuses: Vec<StatusCheck>,
    /// Other runs on the commit, such as deployments started by hand; they
    /// don't count towards the PR's checks. Newest first.
    pub others: Vec<CheckSuite>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckSuite {
    /// The workflow's name, or the app's for checks from outside Actions.
    pub name: String,
    pub state: RunState,
    /// Set for GitHub Actions, whose job logs the app can show.
    pub run: Option<WorkflowRun>,
    pub checks: Vec<CheckRun>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub number: u32,
    /// What triggered it, e.g. `pull_request`.
    pub event: String,
    pub url: String,
}

/// One check; for GitHub Actions, a job, with the same id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    pub state: RunState,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusCheck {
    pub context: String,
    pub state: RunState,
    pub description: Option<String>,
    pub url: Option<String>,
}

/// A GitHub Actions job, with its steps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub run_id: u64,
    /// Re-running jobs starts a new attempt, with new job ids.
    pub run_attempt: u32,
    pub name: String,
    pub workflow: String,
    pub state: RunState,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub url: String,
    pub steps: Vec<JobStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobStep {
    pub number: u32,
    pub name: String,
    pub state: RunState,
    /// Whole seconds, like every timestamp GitHub's API returns.
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// A GitHub Actions link's repository, run and job:
/// `https://github.com/o/r/actions/runs/1/job/2` is `("o/r", 1, Some(2))`.
pub fn parse_actions_url(url: &str) -> Option<(String, u64, Option<u64>)> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix('/'))?;
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if (parts.next()?, parts.next()?) != ("actions", "runs") {
        return None;
    }
    let run = parts.next()?.parse().ok()?;
    let job = match (parts.next(), parts.next()) {
        (Some("job"), Some(id)) => Some(id.parse().ok()?),
        _ => None,
    };
    Some((format!("{owner}/{name}"), run, job))
}

/// One step's share of a job's log.
#[derive(Clone, Debug, PartialEq)]
pub struct StepLog {
    /// The step's number, or 0 for a log that couldn't be split into steps.
    pub number: u32,
    pub lines: Vec<LogLine>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// The text without its timestamp, workflow command or escape codes.
    pub text: String,
    /// Terminal colours, as byte ranges of `text`.
    pub styles: Vec<(u32, u32, Style)>,
    pub kind: LogKind,
    /// How many groups the line is in. A group's lines follow its header,
    /// one deeper, up to the next line that is no deeper than the header.
    pub depth: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    Plain,
    /// The header of a collapsible group.
    Group,
    Command,
    Error,
    Warning,
    Notice,
    Debug,
    Section,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// One of the terminal's 256 colours; 0–15 follow its theme.
    Index(u8),
    Rgb(u8, u8, u8),
}

/// Lines the runner starts its own steps with; along with `Run …` group
/// headers, they mark where the next step's output begins.
const STEP_OPENERS: &[&str] = &[
    "Post job cleanup.",
    "Cleaning up orphan processes",
    "Evaluate and set job outputs",
    "Evaluate and set environment url",
];

/// Splits a job's raw log into its steps.
///
/// The log has no step markers, only per-line timestamps, and the API gives
/// step times in whole seconds, so quick steps share a second. Timestamps
/// decide which steps a line could belong to; the `Run …` header every action
/// and script step opens with breaks the tie. Composite actions print such
/// headers for their own steps too: newer runners wrap those in
/// `start-action`/`end-action`, older ones are told apart by time alone.
pub fn parse_log(raw: &str, steps: &[JobStep]) -> Vec<StepLog> {
    // Skipped steps print nothing, but share their second with the next one.
    let windows: Vec<(u32, i64, i64)> = steps
        .iter()
        .filter(|step| step.state != RunState::Skipped)
        .filter_map(|step| {
            let start = epoch_seconds(step.started_at.as_deref()?)?;
            let end = step
                .completed_at
                .as_deref()
                .and_then(epoch_seconds)
                .unwrap_or(i64::MAX);
            Some((step.number, start, end))
        })
        .collect();
    let mut logs: Vec<StepLog> = if windows.is_empty() {
        vec![StepLog {
            number: 0,
            lines: Vec::new(),
        }]
    } else {
        windows
            .iter()
            .map(|&(number, _, _)| StepLog {
                number,
                lines: Vec::new(),
            })
            .collect()
    };

    let mut current = 0;
    let mut groups: u8 = 0;
    let mut actions: u32 = 0;
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    for line in raw.lines() {
        let (time, rest) = split_timestamp(line);
        // A carriage return redraws the line, as in a terminal.
        let rest = rest
            .rsplit('\r')
            .find(|part| !part.is_empty())
            .unwrap_or("");
        let (directive, body) = directive(rest);
        if let Some(time) = time.filter(|_| !windows.is_empty()) {
            let opens_step = actions == 0
                && match directive {
                    Directive::Group => body.starts_with("Run "),
                    Directive::Line(LogKind::Plain) => STEP_OPENERS.contains(&body),
                    _ => false,
                };
            let next = next_step(&windows, current, time, opens_step);
            if next != current {
                current = next;
                groups = 0;
                actions = 0;
            }
        }
        let kind = match directive {
            Directive::StartAction => {
                actions += 1;
                continue;
            }
            Directive::EndAction => {
                actions = actions.saturating_sub(1);
                continue;
            }
            Directive::EndGroup => {
                groups = groups.saturating_sub(1);
                continue;
            }
            Directive::Group => LogKind::Group,
            Directive::Line(kind) => kind,
        };
        let (text, styles) = log_text(kind, body);
        logs[current].lines.push(LogLine {
            text,
            styles,
            kind,
            depth: groups,
        });
        if kind == LogKind::Group {
            groups = groups.saturating_add(1);
        }
    }
    logs
}

/// Which step a line at `time` belongs to, given the one before it did to
/// `current`. Steps only ever move forward.
fn next_step(windows: &[(u32, i64, i64)], current: usize, time: i64, opens_step: bool) -> usize {
    let mut later = current + 1..windows.len();
    if opens_step
        && let Some(next) = later
            .clone()
            .find(|&n| windows[n].1 <= time && time <= windows[n].2)
    {
        return next;
    }
    if time > windows[current].2
        && let Some(next) = later.find(|&n| time <= windows[n].2)
    {
        return next;
    }
    current
}

/// `2026-10-01T14:31:59.6942703Z text` is the line's time in whole seconds
/// and its text.
fn split_timestamp(line: &str) -> (Option<i64>, &str) {
    match line.split_once(' ') {
        Some((stamp, rest)) if stamp.len() >= 20 && stamp.ends_with('Z') => {
            match epoch_seconds(stamp) {
                Some(time) => (Some(time), rest),
                None => (None, line),
            }
        }
        _ => (None, line),
    }
}

/// Seconds since the Unix epoch of a UTC `YYYY-MM-DDTHH:MM:SS…` timestamp;
/// fractions of a second are dropped.
pub fn epoch_seconds(timestamp: &str) -> Option<i64> {
    let bytes = timestamp.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |at: usize, len: usize| -> Option<i64> {
        let digits = timestamp.get(at..at + len)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())?
    };
    let (year, month, day) = (number(0, 4)?, number(5, 2)?, number(8, 2)?);
    let (hour, minute, second) = (number(11, 2)?, number(14, 2)?, number(17, 2)?);
    // Days from the civil date, after Howard Hinnant's algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Directive {
    Line(LogKind),
    Group,
    EndGroup,
    StartAction,
    EndAction,
}

/// The runner's `##[…]` workflow commands, and `[command]`, which
/// `@actions/exec` prints before each program it runs.
fn directive(text: &str) -> (Directive, &str) {
    if let Some(body) = text.strip_prefix("[command]") {
        return (Directive::Line(LogKind::Command), body);
    }
    let Some((name, body)) = text
        .strip_prefix("##[")
        .and_then(|rest| rest.split_once(']'))
    else {
        return (Directive::Line(LogKind::Plain), text);
    };
    let directive = match name {
        "group" => Directive::Group,
        "endgroup" => Directive::EndGroup,
        "command" => Directive::Line(LogKind::Command),
        "error" => Directive::Line(LogKind::Error),
        "warning" => Directive::Line(LogKind::Warning),
        "notice" => Directive::Line(LogKind::Notice),
        "debug" => Directive::Line(LogKind::Debug),
        "section" => Directive::Line(LogKind::Section),
        _ if name.starts_with("start-action") => Directive::StartAction,
        _ if name.starts_with("end-action") => Directive::EndAction,
        _ => return (Directive::Line(LogKind::Plain), text),
    };
    (directive, body)
}

/// A line's text and colours. Annotations read the way GitHub shows them,
/// as `Error: …`.
fn log_text(kind: LogKind, body: &str) -> (String, Vec<(u32, u32, Style)>) {
    let (text, mut styles) = parse_ansi(body);
    let label = match kind {
        LogKind::Error => "Error",
        LogKind::Warning => "Warning",
        LogKind::Notice => "Notice",
        _ => return (text, styles),
    };
    let labelled = text
        .get(..label.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(label));
    if labelled {
        return (text, styles);
    }
    let prefix = format!("{label}: ");
    let shift = prefix.len() as u32;
    for (start, end, _) in &mut styles {
        *start += shift;
        *end += shift;
    }
    (prefix + &text, styles)
}

/// Strips terminal escape codes, keeping the colours they set as styled
/// byte ranges of the text that is left.
pub fn parse_ansi(input: &str) -> (String, Vec<(u32, u32, Style)>) {
    let bytes = input.as_bytes();
    let mut text = String::with_capacity(input.len());
    let mut styles = Vec::new();
    let mut style = Style::default();
    let mut styled_from = 0;
    let mut plain_from = 0;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte != 0x1b && (byte >= 0x20 || byte == b'\t') {
            i += 1;
            continue;
        }
        // Every byte that ends a run of text here is ASCII, so the slices
        // fall on character boundaries.
        text.push_str(&input[plain_from..i]);
        if byte != 0x1b {
            i += 1;
        } else if bytes.get(i + 1) == Some(&b'[') {
            let params = i + 2;
            let mut end = params;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if bytes.get(end) == Some(&b'm') {
                let next = apply_sgr(style, &input[params..end]);
                if next != style {
                    if style != Style::default() && text.len() > styled_from {
                        styles.push((styled_from as u32, text.len() as u32, style));
                    }
                    style = next;
                    styled_from = text.len();
                }
            }
            i = end + 1;
        } else if bytes.get(i + 1) == Some(&b']') {
            // An operating system command, such as a window title or a
            // hyperlink, ends with BEL or ESC \.
            let mut end = i + 2;
            while end < bytes.len() && bytes[end] != 0x07 && bytes[end] != 0x1b {
                end += 1;
            }
            i = if bytes.get(end) == Some(&0x1b) {
                end + 2
            } else {
                end + 1
            };
        } else {
            i += 2;
        }
        i = i.min(bytes.len());
        plain_from = i;
    }
    text.push_str(&input[plain_from..]);
    if style != Style::default() && text.len() > styled_from {
        styles.push((styled_from as u32, text.len() as u32, style));
    }
    (text, styles)
}

/// Applies a Select Graphic Rendition sequence's parameters, e.g. `1;31`.
fn apply_sgr(mut style: Style, params: &str) -> Style {
    let mut codes = params
        .split([';', ':'])
        .map(|code| code.parse::<u16>().unwrap_or(0));
    while let Some(code) = codes.next() {
        match code {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(Color::Index((code - 30) as u8)),
            38 => style.fg = extended_color(&mut codes),
            39 => style.fg = None,
            40..=47 => style.bg = Some(Color::Index((code - 40) as u8)),
            48 => style.bg = extended_color(&mut codes),
            49 => style.bg = None,
            90..=97 => style.fg = Some(Color::Index((code - 90 + 8) as u8)),
            100..=107 => style.bg = Some(Color::Index((code - 100 + 8) as u8)),
            _ => {}
        }
    }
    style
}

/// `5;n` picks one of 256 colours, `2;r;g;b` any colour.
fn extended_color(codes: &mut impl Iterator<Item = u16>) -> Option<Color> {
    let byte = |code: u16| code.min(255) as u8;
    match codes.next()? {
        5 => Some(Color::Index(byte(codes.next()?))),
        2 => Some(Color::Rgb(
            byte(codes.next()?),
            byte(codes.next()?),
            byte(codes.next()?),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(number: u32, start: &str, end: &str, state: RunState) -> JobStep {
        JobStep {
            number,
            name: format!("step {number}"),
            state,
            started_at: Some(format!("2026-10-01T12:00:{start}Z")),
            completed_at: Some(format!("2026-10-01T12:00:{end}Z")),
        }
    }

    fn texts(log: &StepLog) -> Vec<&str> {
        log.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn maps_status_and_conclusion() {
        assert_eq!(
            RunState::from_github("COMPLETED", Some("SUCCESS")),
            RunState::Success
        );
        assert_eq!(
            RunState::from_github("completed", Some("timed_out")),
            RunState::Failure
        );
        assert_eq!(
            RunState::from_github("IN_PROGRESS", None),
            RunState::InProgress
        );
        assert_eq!(RunState::from_github("waiting", None), RunState::Queued);
        assert_eq!(RunState::from_status("ERROR"), RunState::Failure);
        assert_eq!(RunState::from_status("PENDING"), RunState::InProgress);
    }

    #[test]
    fn parses_actions_links() {
        assert_eq!(
            parse_actions_url("https://github.com/o/r/actions/runs/12/job/34?pr=5#step:3:1"),
            Some(("o/r".into(), 12, Some(34)))
        );
        assert_eq!(
            parse_actions_url("https://github.com/o/r/actions/runs/12"),
            Some(("o/r".into(), 12, None))
        );
        assert_eq!(
            parse_actions_url("/o/r/actions/runs/12/attempts/2"),
            Some(("o/r".into(), 12, None))
        );
        assert_eq!(parse_actions_url("https://github.com/o/r/pull/12"), None);
        assert_eq!(
            parse_actions_url("https://github.com/o/r/actions/runs/x"),
            None
        );
    }

    #[test]
    fn reads_timestamps() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_seconds("2026-10-01T14:31:59Z"), Some(1_790_865_119));
        assert_eq!(
            epoch_seconds("2026-10-01T14:31:59.6942703Z"),
            Some(1_790_865_119)
        );
        assert_eq!(epoch_seconds("2026-10-01 14:31:59"), None);
        assert_eq!(epoch_seconds("2026-1x-01T14:31:59Z"), None);
    }

    #[test]
    fn keeps_colours_as_styles() {
        let (text, styles) = parse_ansi("\x1b[0m\x1b[1mmodule.x\x1b[0m: \x1b[31m-\x1b[0m gone");
        assert_eq!(text, "module.x: - gone");
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let red = Style {
            fg: Some(Color::Index(1)),
            ..Style::default()
        };
        assert_eq!(styles, [(0, 8, bold), (10, 11, red)]);

        let (text, styles) = parse_ansi("\x1b[38;5;208mo\x1b[48;2;1;2;3mk\x1b[K\x1b]0;title\x07!");
        assert_eq!(text, "ok!");
        assert_eq!(styles[0].2.fg, Some(Color::Index(208)));
        assert_eq!(styles[1].2.bg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(styles[1].1, 3, "the style runs to the end of the line");
    }

    #[test]
    fn reads_workflow_commands() {
        let raw = "\u{feff}2026-10-01T12:00:00.1Z ##[group]Run cargo test\n\
                   2026-10-01T12:00:00.2Z \x1b[36;1mcargo test\x1b[0m\n\
                   2026-10-01T12:00:00.3Z ##[endgroup]\n\
                   2026-10-01T12:00:00.4Z [command]/usr/bin/git version\n\
                   2026-10-01T12:00:00.5Z ##[error]Process completed with exit code 1.\n\
                   2026-10-01T12:00:00.6Z ##[warning]Warning: already labelled\n\
                   2026-10-01T12:00:00.7Z 10%\r50%\r100%\r\n";
        let logs = parse_log(raw, &[]);
        assert_eq!(logs.len(), 1);
        let lines = &logs[0].lines;
        assert_eq!(
            texts(&logs[0]),
            [
                "Run cargo test",
                "cargo test",
                "/usr/bin/git version",
                "Error: Process completed with exit code 1.",
                "Warning: already labelled",
                "100%",
            ]
        );
        let kinds: Vec<_> = lines.iter().map(|l| (l.kind, l.depth)).collect();
        assert_eq!(
            kinds,
            [
                (LogKind::Group, 0),
                (LogKind::Plain, 1),
                (LogKind::Command, 0),
                (LogKind::Error, 0),
                (LogKind::Warning, 0),
                (LogKind::Plain, 0),
            ]
        );
    }

    #[test]
    fn nests_groups() {
        let raw = "2026-10-01T12:00:00Z ##[group]Get secrets\n\
                   2026-10-01T12:00:00Z ##[group]Token info\n\
                   2026-10-01T12:00:00Z ttl 1h\n\
                   2026-10-01T12:00:00Z ##[endgroup]\n\
                   2026-10-01T12:00:00Z ##[endgroup]\n\
                   2026-10-01T12:00:00Z done\n";
        let depths: Vec<u8> = parse_log(raw, &[])[0]
            .lines
            .iter()
            .map(|l| l.depth)
            .collect();
        assert_eq!(depths, [0, 1, 2, 0]);
    }

    #[test]
    fn splits_steps_that_share_a_second() {
        // Modelled on a real job: steps 3 and 4 start in the second step 2
        // ends in, and the skipped step 5 shares one with step 6.
        let steps = [
            step(1, "00", "04", RunState::Success),
            step(2, "04", "05", RunState::Success),
            step(3, "05", "05", RunState::Success),
            step(4, "05", "07", RunState::Success),
            step(5, "09", "09", RunState::Skipped),
            step(6, "09", "09", RunState::Success),
            step(7, "09", "10", RunState::Success),
            step(8, "10", "11", RunState::Success),
            step(9, "11", "11", RunState::Success),
        ];
        let raw = "2026-10-01T12:00:00.69Z Current runner version: '2.314.1'\n\
                   2026-10-01T12:00:04.60Z ##[group]Run actions/checkout@v4\n\
                   2026-10-01T12:00:04.61Z ##[endgroup]\n\
                   2026-10-01T12:00:05.10Z checked out\n\
                   2026-10-01T12:00:05.59Z ##[group]Run actions/setup-node@v4\n\
                   2026-10-01T12:00:05.60Z ##[endgroup]\n\
                   2026-10-01T12:00:05.94Z ##[group]Run hashicorp/setup-terraform@v3\n\
                   2026-10-01T12:00:05.95Z ##[endgroup]\n\
                   2026-10-01T12:00:06.80Z unzip\n\
                   2026-10-01T12:00:09.63Z ##[group]Run rm -rf .terraform\n\
                   2026-10-01T12:00:09.64Z ##[endgroup]\n\
                   2026-10-01T12:00:09.94Z Post job cleanup.\n\
                   2026-10-01T12:00:10.10Z cache saved\n\
                   2026-10-01T12:00:10.46Z ##[group]Run '/srv/post_cleanup.sh'\n\
                   2026-10-01T12:00:10.47Z ##[endgroup]\n\
                   2026-10-01T12:00:11.29Z Evaluate and set environment url\n\
                   2026-10-01T12:00:11.30Z Cleaning up orphan processes\n";
        let logs = parse_log(raw, &steps);
        let by_step: Vec<(u32, Vec<&str>)> = logs.iter().map(|l| (l.number, texts(l))).collect();
        assert_eq!(
            by_step,
            [
                (1, vec!["Current runner version: '2.314.1'"]),
                (2, vec!["Run actions/checkout@v4", "checked out"]),
                (3, vec!["Run actions/setup-node@v4"]),
                (4, vec!["Run hashicorp/setup-terraform@v3", "unzip"]),
                (6, vec!["Run rm -rf .terraform"]),
                (7, vec!["Post job cleanup.", "cache saved"]),
                (8, vec!["Run '/srv/post_cleanup.sh'"]),
                (
                    9,
                    vec![
                        "Evaluate and set environment url",
                        "Cleaning up orphan processes"
                    ]
                ),
            ]
        );
    }

    #[test]
    fn keeps_composite_actions_in_their_step() {
        // The composite's own `Run` header falls in the second the next step
        // starts in; `start-action` says it is still the composite's.
        let steps = [
            step(1, "23", "24", RunState::Success),
            step(2, "24", "31", RunState::Success),
        ];
        let raw = "2026-10-01T12:00:23.45Z ##[group]Run org/actions/setup@v1\n\
                   2026-10-01T12:00:23.46Z ##[endgroup]\n\
                   2026-10-01T12:00:24.51Z ##[start-action display=Configure;id=__self.export]\n\
                   2026-10-01T12:00:24.52Z ##[group]Run actions/github-script@v7\n\
                   2026-10-01T12:00:24.53Z ##[endgroup]\n\
                   2026-10-01T12:00:24.61Z ##[end-action id=__self.export;outcome=success]\n\
                   2026-10-01T12:00:24.62Z ##[group]Run ruby/setup-ruby@v1\n";
        let logs = parse_log(raw, &steps);
        assert_eq!(
            texts(&logs[0]),
            ["Run org/actions/setup@v1", "Run actions/github-script@v7"]
        );
        assert_eq!(texts(&logs[1]), ["Run ruby/setup-ruby@v1"]);
    }
}
