//! Read-only access to a pull request's code for the Code tab: its file list,
//! file contents, and `git grep`.
//!
//! Everything addresses the PR's head commit in the repository's clone, not
//! the files in its worktree, so results stay exact even after you edit the
//! worktree in Zed. Syncing a worktree makes the head's file contents local.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use shared::code::is_symbol;
use shared::{
    CodeTree, DiffShas, FileBody, FileContent, GrepMatch, GrepQuery, GrepResult, LineHighlights,
    is_branch_name,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::error::{Error, Result};
use crate::git::Git;
use crate::highlight;
use crate::workspace::Workspace;

/// Bigger files are left to Zed.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILE_LINES: usize = 20_000;
const MAX_MATCHES: usize = 2_000;
const MAX_DEFINITIONS: usize = 200;
/// Lines a symbol search reads to find definitions among the uses.
const MAX_SCANNED: usize = 20_000;
/// Longer lines are shortened around the match.
const MAX_LINE: usize = 300;

/// Set once `git grep -P` turned out to be unavailable (git built without
/// PCRE); regex searches then use POSIX extended regexes.
static NO_PCRE: AtomicBool = AtomicBool::new(false);

/// The files at a pull request's head.
pub async fn tree(git: &Git, workspace: &Workspace, repo: &str, number: u32) -> Result<CodeTree> {
    let dir = clone_dir(workspace, repo)?;
    let sha = head_of(git, &dir, number).await?;
    let listing = git
        .run(
            &dir,
            &[
                "ls-tree",
                "-r",
                "-z",
                "--format=%(objecttype) %(path)",
                &sha,
            ],
        )
        .await?;
    let paths = listing
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("blob "))
        .map(str::to_string)
        .collect();
    Ok(CodeTree { sha, paths })
}

pub async fn read_file(
    git: &Git,
    workspace: &Workspace,
    repo: &str,
    sha: &str,
    path: &str,
) -> Result<FileContent> {
    check_sha(sha)?;
    check_path(path)?;
    let dir = clone_dir(workspace, repo)?;
    let object = format!("{sha}:{path}");
    let size: u64 = git
        .run(&dir, &["cat-file", "-s", &object])
        .await
        .map_err(|_| Error::msg(format!("{path} doesn't exist at {}.", &sha[..7])))?
        .trim()
        .parse()
        .map_err(|_| Error::msg("git cat-file returned no size"))?;
    let content = |body| FileContent {
        path: path.to_string(),
        sha: sha.to_string(),
        size,
        body,
        highlights: Vec::new(),
    };
    if size > MAX_FILE_BYTES {
        return Ok(content(FileBody::TooLarge));
    }
    let bytes = git.run_bytes(&dir, &["cat-file", "blob", &object]).await?;
    // Git's own test: a NUL byte early on means binary.
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Ok(content(FileBody::Binary));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.lines().count() > MAX_FILE_LINES {
        return Ok(content(FileBody::TooLarge));
    }
    // Parsing is CPU work, so it runs off the async threads.
    let (text, highlights) = {
        let path = path.to_string();
        tokio::task::spawn_blocking(move || {
            let highlights = highlight::highlight(&path, &text);
            (text, highlights)
        })
        .await
        .map_err(|e| Error::msg(e.to_string()))?
    };
    Ok(FileContent {
        highlights,
        ..content(FileBody::Text(text))
    })
}

pub async fn grep(
    git: &Git,
    workspace: &Workspace,
    repo: &str,
    sha: &str,
    query: &GrepQuery,
) -> Result<GrepResult> {
    check_sha(sha)?;
    let dir = clone_dir(workspace, repo)?;
    // Several lines would be several patterns to git; search the first.
    let pattern = query.pattern.lines().next().unwrap_or_default();
    if pattern.trim().is_empty() {
        return Ok(GrepResult::default());
    }
    let paths = pathspecs(&query.paths);
    let search = Search {
        git,
        workspace,
        repo,
        dir: &dir,
        sha,
        paths: &paths,
    };

    if query.symbol && is_symbol(pattern) {
        // Every whole-word use, then sorted into definitions and the rest.
        // One fixed-string grep plus the `regex` crate's linear-time matching
        // is fast; asking git for the definition patterns took seconds on
        // a big repo (POSIX) or hit PCRE's backtracking limit.
        let uses = Grep {
            flavor: Flavor::Fixed,
            patterns: vec![pattern.to_string()],
            ignore_case: false,
            whole_word: true,
            limit: MAX_SCANNED,
        };
        let (hits, scan_truncated) = search.run(&uses).await?;
        let defines = definition_regex(pattern);
        let (definitions, references): (Vec<&Hit>, Vec<&Hit>) =
            hits.iter().partition(|hit| defines.is_match(&hit.text));
        let symbol = Highlight::word(pattern);
        return Ok(GrepResult {
            definitions: definitions
                .iter()
                .take(MAX_DEFINITIONS)
                .map(|hit| symbol.apply(hit))
                .collect(),
            truncated: scan_truncated || references.len() > MAX_MATCHES,
            matches: references
                .iter()
                .take(MAX_MATCHES)
                .map(|hit| symbol.apply(hit))
                .collect(),
        });
    }

    let flavor = match (query.regex, NO_PCRE.load(Ordering::Relaxed)) {
        (false, _) => Flavor::Fixed,
        (true, false) => Flavor::Perl,
        (true, true) => Flavor::Extended,
    };
    let mut grep = Grep {
        flavor,
        patterns: vec![pattern.to_string()],
        ignore_case: !query.case_sensitive,
        whole_word: query.whole_word,
        limit: MAX_MATCHES,
    };
    let (hits, truncated) = match search.run(&grep).await {
        Err(error) if flavor == Flavor::Perl && is_pcre_missing(&error) => {
            NO_PCRE.store(true, Ordering::Relaxed);
            grep.flavor = Flavor::Extended;
            search.run(&grep).await?
        }
        result => result?,
    };
    let highlight = Highlight::new(pattern, query);
    Ok(GrepResult {
        definitions: Vec::new(),
        matches: hits.iter().map(|hit| highlight.apply(hit)).collect(),
        truncated,
    })
}

/// What the diff compares: the PR head and its merge base with `base_ref`.
/// Also makes the old versions of `paths` local in one batch, so the diff's
/// removed lines can be highlighted without fetching file by file.
pub async fn diff_shas(
    git: &Git,
    workspace: &Workspace,
    repo: &str,
    number: u32,
    base_ref: &str,
    paths: &[String],
) -> Result<DiffShas> {
    if !is_branch_name(base_ref) {
        return Err(Error::msg(format!("Not a branch: {base_ref:?}")));
    }
    for path in paths {
        check_path(path)?;
    }
    let dir = clone_dir(workspace, repo)?;
    let head = head_of(git, &dir, number).await?;
    let base = git
        .run(
            &dir,
            &[
                "merge-base",
                &format!("refs/remotes/origin/{base_ref}"),
                &head,
            ],
        )
        .await?
        .trim()
        .to_string();
    // Best effort: without it, each old file arrives when it's highlighted.
    let _ = workspace.ensure_files(git, repo, &base, paths).await;
    Ok(DiffShas { head, base })
}

/// Syntax highlighting for the lines of `path` at `sha` that fall in
/// `ranges` (inclusive line numbers), for the diff. Empty for binary, very
/// large or unknown files.
pub async fn highlight_lines(
    git: &Git,
    workspace: &Workspace,
    repo: &str,
    sha: &str,
    path: &str,
    ranges: &[(u32, u32)],
) -> Result<LineHighlights> {
    check_sha(sha)?;
    check_path(path)?;
    let dir = clone_dir(workspace, repo)?;
    let object = format!("{sha}:{path}");
    let bytes = match git
        .offline()
        .run_bytes(&dir, &["cat-file", "blob", &object])
        .await
    {
        Ok(bytes) => bytes,
        // Not local after all: fetch just this one.
        Err(_) => git.run_bytes(&dir, &["cat-file", "blob", &object]).await?,
    };
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes.iter().take(8000).any(|&b| b == 0) {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (path, ranges) = (path.to_string(), ranges.to_vec());
    tokio::task::spawn_blocking(move || {
        let spans = highlight::highlight(&path, &text);
        lines_of(&text, &spans, &ranges)
    })
    .await
    .map_err(|e| Error::msg(e.to_string()))
}

/// Groups flat spans by line, relative to each line's start, keeping the
/// lines in `ranges`.
fn lines_of(text: &str, spans: &[u32], ranges: &[(u32, u32)]) -> LineHighlights {
    let starts: Vec<u32> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i as u32 + 1))
        .collect();
    let wanted = |line: u32| ranges.iter().any(|&(a, b)| a <= line && line <= b);
    let mut lines: LineHighlights = Vec::new();
    for &[from, to, kind] in spans.as_chunks::<3>().0 {
        let index = starts.partition_point(|&start| start <= from) - 1;
        let line = index as u32 + 1;
        if !wanted(line) {
            continue;
        }
        let start = starts[index];
        let relative = [from - start, to - start, kind];
        match lines.last_mut() {
            Some((last, list)) if *last == line => list.extend(relative),
            _ => lines.push((line, relative.to_vec())),
        }
    }
    lines
}

async fn head_of(git: &Git, dir: &Path, number: u32) -> Result<String> {
    Ok(git
        .run(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/pull/{number}/head^{{commit}}"),
            ],
        )
        .await
        .map_err(|_| Error::msg(format!("pr-{number} isn't synced yet.")))?
        .trim()
        .to_string())
}

fn clone_dir(workspace: &Workspace, repo: &str) -> Result<PathBuf> {
    let dir = workspace.repo_dir(repo)?;
    if dir.join(".bare").exists() {
        Ok(dir)
    } else {
        Err(Error::msg(format!("{repo} isn't cloned yet.")))
    }
}

/// Shas come back from the UI, which got them from [`tree`].
fn check_sha(sha: &str) -> Result<()> {
    if matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::msg(format!("Not a commit: {sha:?}")))
    }
}

fn check_path(path: &str) -> Result<()> {
    let inside = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if inside {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "Not a file in the repository: {path:?}"
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    Fixed,
    Extended,
    Perl,
}

struct Grep {
    flavor: Flavor,
    /// Any of these, as separate `-e` arguments.
    patterns: Vec<String>,
    ignore_case: bool,
    whole_word: bool,
    limit: usize,
}

/// A matching line as git reports it.
#[derive(Debug, PartialEq)]
struct Hit {
    path: String,
    line: u32,
    /// 1-based byte offset of the first match.
    column: usize,
    text: String,
}

struct Search<'a> {
    git: &'a Git,
    workspace: &'a Workspace,
    repo: &'a str,
    dir: &'a Path,
    sha: &'a str,
    paths: &'a [String],
}

impl Search<'_> {
    /// Greps without downloading anything. Should file contents be missing
    /// after all (a worktree that never finished syncing), they are fetched
    /// in one batch and the search runs again.
    async fn run(&self, grep: &Grep) -> Result<(Vec<Hit>, bool)> {
        let offline = self.git.offline();
        match run_grep(&offline, self.dir, self.sha, self.paths, grep).await {
            Err(error) if is_missing_object(&error) => {
                self.workspace
                    .ensure_contents(self.git, self.repo, self.sha)
                    .await?;
                run_grep(&offline, self.dir, self.sha, self.paths, grep).await
            }
            result => result,
        }
    }
}

/// Streams `git grep`'s output and stops it once `grep.limit` lines matched.
/// Returns the matches and whether there were more.
async fn run_grep(
    git: &Git,
    dir: &Path,
    sha: &str,
    paths: &[String],
    grep: &Grep,
) -> Result<(Vec<Hit>, bool)> {
    let mut args = vec![
        "grep",
        "--line-number",
        "--column",
        "-I",
        "-z",
        "--full-name",
        "--no-color",
        match grep.flavor {
            Flavor::Fixed => "-F",
            Flavor::Extended => "-E",
            Flavor::Perl => "-P",
        },
    ];
    if grep.ignore_case {
        args.push("-i");
    }
    if grep.whole_word {
        args.push("-w");
    }
    // Patterns always follow -e, so one starting with "-" is never an option.
    for pattern in &grep.patterns {
        args.extend(["-e", pattern.as_str()]);
    }
    args.extend([sha, "--"]);
    args.extend(paths.iter().map(String::as_str));

    let mut child = git
        .command(dir)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let prefix = format!("{sha}:");

    let read_matches = async {
        let mut reader = BufReader::new(stdout);
        let mut hits = Vec::new();
        let mut record = Vec::new();
        loop {
            let read = reader.read_until(b'\n', &mut record).await?;
            // A newline inside a file name ends the read early; a record is
            // whole once its three NUL separators are in.
            if read > 0 && record.iter().filter(|&&b| b == 0).count() < 3 {
                continue;
            }
            if let Some(hit) = parse_record(&record, &prefix) {
                hits.push(hit);
            }
            record.clear();
            if read == 0 || hits.len() > grep.limit {
                break;
            }
        }
        let truncated = hits.len() > grep.limit;
        if truncated {
            hits.truncate(grep.limit);
            let _ = child.start_kill();
        }
        Ok::<_, std::io::Error>((hits, truncated))
    };
    let read_errors = async {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer).await;
        buffer
    };
    let (matches, errors) = tokio::join!(read_matches, read_errors);
    let (hits, truncated) = matches?;
    let status = child.wait().await?;
    // Exit code 1 means nothing matched.
    if !truncated && !matches!(status.code(), Some(0 | 1)) {
        let message = String::from_utf8_lossy(&errors);
        let message = message.trim().trim_start_matches("fatal: ");
        return Err(Error::msg(format!("git grep: {message}")));
    }
    Ok((hits, truncated))
}

/// `<sha>:<path>\0<line>\0<column>\0<text>\n`.
fn parse_record(record: &[u8], prefix: &str) -> Option<Hit> {
    let mut fields = record.splitn(4, |&b| b == 0);
    let name = String::from_utf8_lossy(fields.next()?);
    let line = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let column = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let text = fields.next()?;
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    let text = text.strip_suffix(b"\r").unwrap_or(text);
    Some(Hit {
        path: name.strip_prefix(prefix)?.to_string(),
        line,
        column,
        text: String::from_utf8_lossy(text).into_owned(),
    })
}

fn is_missing_object(error: &Error) -> bool {
    let message = error.to_string();
    ["unable to read", "could not read", "missing", "bad object"]
        .iter()
        .any(|needle| message.contains(needle))
}

fn is_pcre_missing(error: &Error) -> bool {
    // "cannot use Perl-compatible regexes when not compiled with USE_LIBPCRE"
    error.to_string().contains("USE_LIBPCRE")
}

/// `*.rb, app/**, !spec/**` as git pathspecs. Names without a slash match at
/// any depth, the way a search box is expected to work, and a plain name also
/// matches everything inside a directory of that name.
fn pathspecs(filter: &str) -> Vec<String> {
    let mut specs = Vec::new();
    for item in filter
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
    {
        let (magic, glob) = match item.strip_prefix('!') {
            Some(rest) => ("exclude,glob", rest),
            None => ("glob", item),
        };
        let glob = glob.trim_start_matches("./").trim_start_matches('/');
        let glob = match glob.strip_suffix('/') {
            Some(dir) => format!("{dir}/**"),
            None => glob.to_string(),
        };
        if glob.is_empty() {
            continue;
        }
        let wildcard = glob.contains(['*', '?', '[']);
        let anywhere = !glob.contains('/');
        let mut push = |glob: String| specs.push(format!(":({magic}){glob}"));
        match (anywhere, wildcard) {
            (true, true) => push(format!("**/{glob}")),
            (true, false) => {
                push(format!("**/{glob}"));
                push(format!("**/{glob}/**"));
            }
            (false, _) => push(glob),
        }
    }
    specs
}

/// Whether a line defines `symbol`. A heuristic: keyword definitions in most
/// languages (receivers such as `self.` and Go's `(s *Server)` included),
/// Ruby's attribute and Rails macros, and constants assigned at the start of
/// a line.
fn definition_regex(symbol: &str) -> regex::Regex {
    const KEYWORDS: &str = "def|defp|defmacro|defmodule|fn|fun|func|function|class|module|\
        struct|enum|union|trait|interface|type|typealias|protocol|record|object|const|let|var|\
        val|static|alias|macro_rules!";
    const MACROS: &str = "attr_reader|attr_writer|attr_accessor|cattr_accessor|mattr_accessor|\
        class_attribute|attribute|alias_method|define_method|delegate|scope|has_many|has_one|\
        belongs_to|has_and_belongs_to_many|enum|store_accessor|helper_method|let!?|subject|\
        def_delegators?|memoize";
    let name = regex::escape(symbol);
    let end = "([^A-Za-z0-9_?!]|$)";
    let mut patterns = vec![
        format!(
            "^[[:space:]]*([A-Za-z_]+(\\([A-Za-z_]+\\))?[[:space:]]+)*({KEYWORDS})[[:space:]]+\
             (\\([^)]*\\)[[:space:]]*)?([A-Za-z_][A-Za-z0-9_]*(\\.|::))*{name}{end}"
        ),
        format!("^[[:space:]]*({MACROS})[[:space:](]([^#]*[[:space:],(])?:{name}{end}"),
    ];
    if symbol.starts_with(|c: char| c.is_ascii_uppercase()) {
        patterns.push(format!("^[[:space:]]*{name}[[:space:]]*=([^=~]|$)"));
    }
    regex::Regex::new(&patterns.join("|")).expect("definition patterns are valid")
}

/// Finds what to highlight in a matching line. Git only reports the first
/// match's column, so the `regex` crate finds them all; when it can't parse a
/// pattern git accepted (PCRE look-arounds), the first match is marked from
/// git's column.
struct Highlight {
    regex: Option<regex::Regex>,
    whole_word: bool,
}

impl Highlight {
    fn new(pattern: &str, query: &GrepQuery) -> Self {
        let source = if query.regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let regex = regex::RegexBuilder::new(&source)
            .case_insensitive(!query.case_sensitive)
            .build()
            .ok();
        Highlight {
            regex,
            whole_word: query.whole_word,
        }
    }

    fn word(symbol: &str) -> Self {
        Highlight {
            regex: regex::Regex::new(&regex::escape(symbol)).ok(),
            whole_word: true,
        }
    }

    fn apply(&self, hit: &Hit) -> GrepMatch {
        let text = &hit.text;
        let mut ranges: Vec<(usize, usize)> = self
            .regex
            .iter()
            .flat_map(|regex| regex.find_iter(text))
            .filter(|m| !m.is_empty())
            .filter(|m| !self.whole_word || is_word_bounded(text, m.start(), m.end()))
            .map(|m| (m.start(), m.end()))
            .collect();
        if ranges.is_empty() {
            let start = hit.column.saturating_sub(1).min(text.len());
            if text.is_char_boundary(start) {
                let end = text[start..]
                    .char_indices()
                    .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                    .map_or(text.len(), |(i, _)| start + i);
                if end > start {
                    ranges.push((start, end));
                }
            }
        }
        let (text, ranges) = excerpt(text, &ranges, MAX_LINE);
        GrepMatch {
            path: hit.path.clone(),
            line: hit.line,
            text,
            ranges,
        }
    }
}

/// Git's `-w`: the match is neither preceded nor followed by a word character.
fn is_word_bounded(text: &str, start: usize, end: usize) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    !text[..start].chars().next_back().is_some_and(word)
        && !text[end..].chars().next().is_some_and(word)
}

/// Shortens `text` to about `max` bytes around the first range, marking cuts
/// with "…", and moves the ranges along.
fn excerpt(text: &str, ranges: &[(usize, usize)], max: usize) -> (String, Vec<(u32, u32)>) {
    let to_u32 = |ranges: &[(usize, usize)]| {
        ranges
            .iter()
            .map(|&(s, e)| (s as u32, e as u32))
            .collect::<Vec<_>>()
    };
    if text.len() <= max {
        return (text.to_string(), to_u32(ranges));
    }
    let floor = |mut i: usize| {
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let focus = ranges.first().map_or(0, |r| r.0);
    let mut start = floor(focus.saturating_sub(max / 3));
    let end = floor((start + max).min(text.len()));
    if end == text.len() {
        start = floor(end.saturating_sub(max));
    }
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < text.len() { "…" } else { "" };
    let shift = prefix.len();
    let moved: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|&&(s, e)| e > start && s < end)
        .map(|&(s, e)| (s.max(start) - start + shift, e.min(end) - start + shift))
        .collect();
    (
        format!("{prefix}{}{suffix}", &text[start..end]),
        to_u32(&moved),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::Fixture;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    /// PR #1 with a small Ruby app, synced into the fixture's workspace.
    fn synced(fx: &Fixture) -> CodeTree {
        let src = fx.src();
        std::fs::create_dir_all(src.join("app/models")).unwrap();
        std::fs::create_dir_all(src.join("vendor/gems")).unwrap();
        std::fs::create_dir_all(src.join("docs/user guide")).unwrap();
        std::fs::write(
            src.join("app/models/session.rb"),
            "class Session\n  SHADOW_TTL = 30\n  attr_reader :shadow_expiry\n\n  \
             def self.shadow_expiry!\n  end\n\n  def tenanted?\n    shadow_expiry.present?\n  end\nend\n",
        )
        .unwrap();
        std::fs::write(
            src.join("app/models/user.rb"),
            "class User < Session\n  def name = \"x\" # shadow_expiry\nend\n",
        )
        .unwrap();
        std::fs::write(src.join("vendor/gems/x.rb"), "shadow_expiry\n").unwrap();
        std::fs::write(src.join("docs/user guide/a b.md"), "Shadow expiry\n").unwrap();
        std::fs::write(src.join("logo.png"), b"\x89PNG\0\0shadow_expiry").unwrap();
        std::fs::write(src.join("big.txt"), "x\n".repeat(MAX_FILE_LINES + 1)).unwrap();
        fx.sh(&src, &["checkout", "--quiet", "-b", "feat/x"]);
        fx.commit("a.txt", "1\n");
        fx.publish("feat/x", 1);

        let (ws, git) = (fx.workspace(), fx.git());
        let pr = fx.pr(1, "feat/x", false);
        block_on(async {
            ws.ensure_repo(&git, "o/r").await.unwrap();
            let previous = ws.fetch(&git, "o/r", &[&pr]).await.unwrap();
            ws.sync_worktree(&git, &pr, previous.get(&1).map(String::as_str))
                .await
                .unwrap();
            tree(&git, &ws, "o/r", 1).await.unwrap()
        })
    }

    fn search(fx: &Fixture, sha: &str, query: GrepQuery) -> Result<GrepResult> {
        block_on(grep(&fx.git(), &fx.workspace(), "o/r", sha, &query))
    }

    fn text(pattern: &str) -> GrepQuery {
        GrepQuery {
            pattern: pattern.into(),
            ..GrepQuery::default()
        }
    }

    fn places(matches: &[GrepMatch]) -> Vec<String> {
        matches
            .iter()
            .map(|m| format!("{}:{}", m.path, m.line))
            .collect()
    }

    #[test]
    fn lists_and_reads_the_head() {
        let fx = Fixture::new();
        let tree = synced(&fx);
        assert_eq!(tree.sha.len(), 40);
        for path in [
            "app/models/session.rb",
            "docs/user guide/a b.md",
            "README.md",
        ] {
            assert!(tree.paths.iter().any(|p| p == path), "{path}");
        }
        let (git, ws) = (fx.git(), fx.workspace());
        let read = |path: &str| block_on(read_file(&git, &ws, "o/r", &tree.sha, path));

        let file = read("docs/user guide/a b.md").unwrap();
        assert_eq!(file.body, FileBody::Text("Shadow expiry\n".into()));
        assert_eq!(read("logo.png").unwrap().body, FileBody::Binary);
        assert_eq!(read("big.txt").unwrap().body, FileBody::TooLarge);
        assert!(read("nope.rb").is_err());
        for bad in ["../secret", "/etc/passwd", "a/../../b", ""] {
            assert!(read(bad).is_err(), "{bad}");
        }
        assert!(block_on(read_file(&git, &ws, "o/r", "HEAD", "README.md")).is_err());
        assert!(block_on(tree_of_unsynced(&fx)).is_err());
    }

    async fn tree_of_unsynced(fx: &Fixture) -> Result<CodeTree> {
        tree(&fx.git(), &fx.workspace(), "o/r", 99).await
    }

    #[test]
    fn greps_with_case_word_regex_and_path_options() {
        let fx = Fixture::new();
        let sha = synced(&fx).sha;

        let result = search(&fx, &sha, text("shadow_expiry")).unwrap();
        assert_eq!(
            places(&result.matches),
            [
                "app/models/session.rb:3",
                "app/models/session.rb:5",
                "app/models/session.rb:9",
                "app/models/user.rb:2",
                "vendor/gems/x.rb:1",
            ],
            "binary files are skipped"
        );
        assert_eq!(result.matches[0].text, "  attr_reader :shadow_expiry");
        assert_eq!(result.matches[0].ranges, [(15, 28)]);
        assert!(!result.truncated);

        let insensitive = search(&fx, &sha, text("shadow EXPIRY")).unwrap();
        assert_eq!(places(&insensitive.matches), ["docs/user guide/a b.md:1"]);
        let sensitive = GrepQuery {
            case_sensitive: true,
            ..text("shadow EXPIRY")
        };
        assert!(search(&fx, &sha, sensitive).unwrap().matches.is_empty());

        let word = GrepQuery {
            whole_word: true,
            ..text("shadow")
        };
        assert_eq!(
            places(&search(&fx, &sha, word).unwrap().matches),
            ["docs/user guide/a b.md:1"]
        );

        let regex = GrepQuery {
            regex: true,
            ..text(r"def\s+self\.\w+")
        };
        let found = search(&fx, &sha, regex).unwrap();
        assert_eq!(places(&found.matches), ["app/models/session.rb:5"]);
        assert_eq!(found.matches[0].ranges, [(2, 24)]);

        let filtered = GrepQuery {
            paths: "*.rb, !vendor/".into(),
            ..text("shadow_expiry")
        };
        assert_eq!(search(&fx, &sha, filtered).unwrap().matches.len(), 4);
        let directory = GrepQuery {
            paths: "gems".into(),
            ..text("shadow_expiry")
        };
        assert_eq!(
            places(&search(&fx, &sha, directory).unwrap().matches),
            ["vendor/gems/x.rb:1"]
        );

        let broken = GrepQuery {
            regex: true,
            ..text("(")
        };
        assert!(search(&fx, &sha, broken).is_err());
        assert!(
            search(&fx, &sha, text("nothing matches this"))
                .unwrap()
                .matches
                .is_empty()
        );
        assert!(search(&fx, "not-a-sha", text("x")).is_err());
    }

    #[test]
    fn finds_definitions_before_uses() {
        let fx = Fixture::new();
        let sha = synced(&fx).sha;
        let symbol = |name: &str| {
            search(
                &fx,
                &sha,
                GrepQuery {
                    symbol: true,
                    ..text(name)
                },
            )
            .unwrap()
        };

        let result = symbol("shadow_expiry");
        assert_eq!(places(&result.definitions), ["app/models/session.rb:3"]);
        // Like git's -w, `self.shadow_expiry!` counts as a use.
        assert_eq!(
            places(&result.matches),
            [
                "app/models/session.rb:5",
                "app/models/session.rb:9",
                "app/models/user.rb:2",
                "vendor/gems/x.rb:1"
            ]
        );
        assert_eq!(result.definitions[0].ranges, [(15, 28)]);

        assert_eq!(
            places(&symbol("shadow_expiry!").definitions),
            ["app/models/session.rb:5"]
        );
        assert_eq!(
            places(&symbol("tenanted?").definitions),
            ["app/models/session.rb:8"]
        );
        assert_eq!(
            places(&symbol("Session").definitions),
            ["app/models/session.rb:1"]
        );
        assert_eq!(
            places(&symbol("SHADOW_TTL").definitions),
            ["app/models/session.rb:2"]
        );
        assert_eq!(
            places(&symbol("name").definitions),
            ["app/models/user.rb:2"]
        );
        assert!(symbol("User").matches.is_empty());
    }

    #[test]
    fn stops_after_the_match_limit() {
        let fx = Fixture::new();
        let sha = synced(&fx).sha;
        let result = search(&fx, &sha, text("x")).unwrap();
        assert!(result.truncated);
        assert_eq!(result.matches.len(), MAX_MATCHES);
    }

    #[test]
    fn highlights_both_sides_of_a_diff() {
        let fx = Fixture::new();
        let src = fx.src();
        std::fs::write(src.join("a.rb"), "def old_name\n  :before\nend\n").unwrap();
        fx.commit("README.md", "base\n");
        fx.sh(&src, &["push", "--quiet", "origin", "main"]);
        let sha = {
            std::fs::write(src.join("a.rb"), "def new_name\n  \"after\"\nend\n").unwrap();
            synced(&fx).sha
        };
        let (git, ws) = (fx.git(), fx.workspace());
        block_on(async {
            let paths = vec!["a.rb".to_string()];
            let shas = diff_shas(&git, &ws, "o/r", 1, "main", &paths)
                .await
                .unwrap();
            assert_eq!(shas.head, sha);
            // The old a.rb was never checked out, yet it's local now.
            let dir = ws.repo_dir("o/r").unwrap();
            let old = format!("{}:a.rb", shas.base);
            let local = git.offline().run(&dir, &["cat-file", "blob", &old]).await;
            assert_eq!(local.unwrap(), "def old_name\n  :before\nend\n");

            let classes = |lines: &LineHighlights, line: u32| -> Vec<&str> {
                let spans = &lines.iter().find(|(n, _)| *n == line).unwrap().1;
                spans
                    .chunks(3)
                    .map(|s| shared::code::HIGHLIGHTS[s[2] as usize].1)
                    .collect()
            };
            let before = highlight_lines(&git, &ws, "o/r", &shas.base, "a.rb", &[(2, 2)])
                .await
                .unwrap();
            assert_eq!(before.len(), 1, "only the lines asked for");
            assert_eq!(classes(&before, 2), ["hl-symbol"]);
            let after = highlight_lines(&git, &ws, "o/r", &shas.head, "a.rb", &[(1, 3)])
                .await
                .unwrap();
            assert_eq!(
                after[0],
                (1, vec![0, 3, after[0].1[2], 4, 12, after[0].1[5]])
            );
            assert!(classes(&after, 2).contains(&"hl-string"));
            assert!(diff_shas(&git, &ws, "o/r", 1, "-x", &paths).await.is_err());
        });
    }

    #[test]
    fn parses_grep_records() {
        let prefix = "abc:";
        let hit = parse_record(b"abc:dir/a\nb.rs\x0012\x005\x00  let x = 1;\r\n", prefix);
        assert_eq!(
            hit,
            Some(Hit {
                path: "dir/a\nb.rs".into(),
                line: 12,
                column: 5,
                text: "  let x = 1;".into(),
            })
        );
        assert_eq!(parse_record(b"", prefix), None);
        assert_eq!(parse_record(b"other:a\x001\x001\x00x\n", prefix), None);
    }

    #[test]
    fn turns_filters_into_pathspecs() {
        assert_eq!(
            pathspecs("*.rb, app/**  !vendor/ ,!spec"),
            [
                ":(glob)**/*.rb",
                ":(glob)app/**",
                ":(exclude,glob)vendor/**",
                ":(exclude,glob)**/spec",
                ":(exclude,glob)**/spec/**",
            ]
        );
        assert!(pathspecs(" , ").is_empty());
        assert_eq!(pathspecs("./lib/x.rb"), [":(glob)lib/x.rb"]);
    }

    #[test]
    fn shortens_long_lines_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
        let (text, ranges) = excerpt(&line, &[(500, 506)], 300);
        assert!(text.starts_with('…') && text.ends_with('…'));
        let (s, e) = ranges[0];
        assert_eq!(&text[s as usize..e as usize], "needle");
        assert!(text.len() <= 300 + 2 * "…".len());

        let tail = format!("{}needle", "é".repeat(400));
        let (text, ranges) = excerpt(&tail, &[(800, 806)], 300);
        assert!(text.ends_with("needle") && text.starts_with('…'));
        let (s, e) = ranges[0];
        assert_eq!(&text[s as usize..e as usize], "needle");

        assert_eq!(
            excerpt("short", &[(0, 5)], 300),
            ("short".into(), vec![(0, 5)])
        );
    }

    #[test]
    fn whole_word_matches_like_git() {
        assert!(is_word_bounded("a foo b", 2, 5));
        assert!(!is_word_bounded("afoo", 1, 4));
        assert!(is_word_bounded("foo?", 0, 3));
    }
}
