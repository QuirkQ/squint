//! Parsing of the per-file unified patches GitHub returns, and the row layouts
//! the UI renders them in.

use serde::{Deserialize, Serialize};

use crate::{Anchor, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Add,
    Del,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
    /// Byte ranges of `text` that differ from the line it replaced or was
    /// replaced by. Empty when the line has no close counterpart.
    pub emphasis: Vec<(u32, u32)>,
    /// The patch marked this line with "\ No newline at end of file".
    pub no_newline: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
    /// GitHub omits the patch for binary files and very large diffs.
    pub patch_missing: bool,
}

impl FileDiff {
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }

    /// The first line worth jumping to in an editor.
    pub fn first_changed_line(&self) -> Option<u32> {
        let lines = self.hunks.iter().flat_map(|h| &h.lines);
        lines
            .clone()
            .find(|l| l.kind == LineKind::Add)
            .and_then(|l| l.new)
            .or_else(|| lines.filter_map(|l| l.new).next())
    }

    /// The lines the hunks show, as inclusive `(first, last)` ranges of the
    /// base and the head version: what highlighting needs to cover.
    pub fn line_ranges(&self) -> (LineRanges, LineRanges) {
        let range = |numbers: &mut dyn Iterator<Item = u32>| {
            let first = numbers.next()?;
            Some(numbers.fold((first, first), |(a, b), n| (a.min(n), b.max(n))))
        };
        let old = self
            .hunks
            .iter()
            .filter_map(|h| range(&mut h.lines.iter().filter_map(|l| l.old)))
            .collect();
        let new = self
            .hunks
            .iter()
            .filter_map(|h| range(&mut h.lines.iter().filter_map(|l| l.new)))
            .collect();
        (old, new)
    }

    /// The hunk, and the index in its lines, of the line a comment is
    /// anchored to, when the diff shows it.
    pub fn locate(&self, anchor: Anchor) -> Option<(usize, usize)> {
        self.hunks.iter().enumerate().find_map(|(h, hunk)| {
            let number = |line: &DiffLine| match anchor.side {
                Side::Old => line.old,
                Side::New => line.new,
            };
            let index = hunk
                .lines
                .iter()
                .position(|l| number(l) == Some(anchor.line))?;
            Some((h, index))
        })
    }

    /// Where this PR changed the head version of the file.
    pub fn head_changes(&self) -> HeadChanges {
        let mut changes = HeadChanges::default();
        for hunk in &self.hunks {
            let mut next = hunk.new_start;
            for line in &hunk.lines {
                match (line.kind, line.new) {
                    (LineKind::Del, _) => {
                        changes.deleted_before.insert(next);
                    }
                    (LineKind::Add, Some(n)) => {
                        changes.added.insert(n);
                        next = n + 1;
                    }
                    (_, Some(n)) => next = n + 1,
                    _ => {}
                }
            }
        }
        changes
    }
}

/// Inclusive `(first, last)` line numbers.
pub type LineRanges = Vec<(u32, u32)>;

/// Head line numbers of a file's changes, for the Code tab's gutter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeadChanges {
    pub added: std::collections::HashSet<u32>,
    /// Lines that removed lines used to precede.
    pub deleted_before: std::collections::HashSet<u32>,
}

impl Hunk {
    /// The head line closest to `self.lines[index]`: its own, or for a
    /// removed line, the one that now takes its place.
    pub fn head_line(&self, index: usize) -> Option<u32> {
        if let Some(n) = self.lines.get(index)?.new {
            return Some(n);
        }
        let after = self.lines[index..].iter().find_map(|l| l.new);
        let before = self.lines[..index].iter().rev().find_map(|l| l.new);
        after
            .or(before.map(|n| n + 1))
            .or(Some(self.new_start.max(1)))
    }

    /// Where a comment on `self.lines[index]` attaches. In the unified view
    /// (`column` is `None`) a context line counts as the head's, as on
    /// GitHub; in the split view it's the column's side.
    pub fn anchor(&self, index: usize, column: Option<Side>) -> Option<Anchor> {
        let line = self.lines.get(index)?;
        let side = match (column, line.kind) {
            (Some(side), _) => side,
            (None, LineKind::Del) => Side::Old,
            (None, _) => Side::New,
        };
        let number = match side {
            Side::Old => line.old,
            Side::New => line.new,
        }?;
        Some(Anchor { side, line: number })
    }

    /// The head's version of lines `first..=last`, which a suggested change
    /// starts from.
    pub fn head_text(&self, first: usize, last: usize) -> String {
        let lines = self.lines.get(first..=last).unwrap_or_default();
        lines
            .iter()
            .filter(|l| l.new.is_some())
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One row of the side-by-side view, as indices into [`Hunk::lines`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitRow {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// Parses the body of a unified diff (the hunks, without `---`/`+++` headers).
pub fn parse_patch(patch: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut old_no, mut new_no) = (0, 0);

    for raw in patch.lines() {
        if raw.starts_with("@@") {
            let (old_start, new_start) = parse_hunk_header(raw).unwrap_or((0, 0));
            (old_no, new_no) = (old_start, new_start);
            hunks.push(Hunk {
                header: raw.to_string(),
                old_start,
                new_start,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            continue;
        };
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &raw[1..]),
            Some(b'-') => (LineKind::Del, &raw[1..]),
            Some(b' ') => (LineKind::Context, &raw[1..]),
            Some(b'\\') => {
                if let Some(last) = hunk.lines.last_mut() {
                    last.no_newline = true;
                }
                continue;
            }
            // Some tools strip the leading space from blank context lines.
            None => (LineKind::Context, ""),
            Some(_) => continue,
        };
        let (old, new) = match kind {
            LineKind::Add => (None, Some(new_no)),
            LineKind::Del => (Some(old_no), None),
            LineKind::Context => (Some(old_no), Some(new_no)),
        };
        old_no += u32::from(old.is_some());
        new_no += u32::from(new.is_some());
        hunk.lines.push(DiffLine {
            kind,
            old,
            new,
            text: text.to_string(),
            emphasis: Vec::new(),
            no_newline: false,
        });
    }

    for hunk in &mut hunks {
        mark_word_changes(&mut hunk.lines);
    }
    hunks
}

/// `@@ -12,7 +12,9 @@ fn context` → `(12, 12)`.
fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace().skip(1);
    let start = |part: &str, sign: char| -> Option<u32> {
        part.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    let old = start(parts.next()?, '-')?;
    let new = start(parts.next()?, '+')?;
    Some((old, new))
}

/// Pairs removed and added lines inside each change block, the way both
/// GitHub and most editors lay out a side-by-side diff.
pub fn split_rows(lines: &[DiffLine]) -> Vec<SplitRow> {
    let mut rows = Vec::with_capacity(lines.len());
    for block in change_blocks(lines) {
        match block {
            Block::Context(i) => rows.push(SplitRow {
                old: Some(i),
                new: Some(i),
            }),
            Block::Change { dels, adds } => {
                for k in 0..dels.len().max(adds.len()) {
                    rows.push(SplitRow {
                        old: dels.clone().nth(k),
                        new: adds.clone().nth(k),
                    });
                }
            }
        }
    }
    rows
}

enum Block {
    Context(usize),
    Change {
        dels: std::ops::Range<usize>,
        adds: std::ops::Range<usize>,
    },
}

fn change_blocks(lines: &[DiffLine]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            blocks.push(Block::Context(i));
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Add {
            i += 1;
        }
        blocks.push(Block::Change {
            dels: del_start..add_start,
            adds: add_start..i,
        });
    }
    blocks
}

fn mark_word_changes(lines: &mut [DiffLine]) {
    for block in change_blocks(lines) {
        let Block::Change { dels, adds } = block else {
            continue;
        };
        for (d, a) in dels.zip(adds) {
            if let Some((old, new)) = word_diff(&lines[d].text, &lines[a].text) {
                lines[d].emphasis = old;
                lines[a].emphasis = new;
            }
        }
    }
}

/// Byte ranges within a line, as stored in [`DiffLine::emphasis`].
type Ranges = Vec<(u32, u32)>;

/// Lines longer than this are emphasised as a whole rather than word by word.
const MAX_TOKENS: usize = 400;

/// Byte ranges that differ between two versions of a line, or `None` when the
/// lines have too little in common for emphasis to help.
fn word_diff(old: &str, new: &str) -> Option<(Ranges, Ranges)> {
    let a = tokenize(old);
    let b = tokenize(new);
    if a.len() > MAX_TOKENS || b.len() > MAX_TOKENS {
        return None;
    }

    // Longest common subsequence over tokens; lines are short, so O(n·m) is fine.
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if a[i].1 == b[j].1 {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }

    let (mut keep_a, mut keep_b) = (vec![false; n], vec![false; m]);
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < n && j < m {
        if a[i].1 == b[j].1 {
            keep_a[i] = true;
            keep_b[j] = true;
            common += a[i].1.trim().len();
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }

    let longest = old.trim().len().max(new.trim().len());
    if longest == 0 || common * 2 < longest {
        return None;
    }
    Some((ranges(&a, &keep_a), ranges(&b, &keep_b)))
}

/// Splits into words, whitespace runs, and single punctuation characters,
/// each paired with its byte offset.
fn tokenize(s: &str) -> Vec<(usize, &str)> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Punct,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Punct
        }
    };

    let mut tokens = Vec::new();
    let mut start = 0;
    let mut prev: Option<Class> = None;
    for (i, c) in s.char_indices() {
        let cls = class(c);
        let joins = prev.as_ref() == Some(&cls) && cls != Class::Punct;
        if !joins && i > start {
            tokens.push((start, &s[start..i]));
            start = i;
        }
        prev = Some(cls);
    }
    if start < s.len() {
        tokens.push((start, &s[start..]));
    }
    tokens
}

fn ranges(tokens: &[(usize, &str)], keep: &[bool]) -> Ranges {
    let mut out = Ranges::new();
    for (&(offset, text), &kept) in tokens.iter().zip(keep) {
        if kept {
            continue;
        }
        let (start, end) = (offset as u32, (offset + text.len()) as u32);
        match out.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => out.push((start, end)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
@@ -1,4 +1,5 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 let d = 5;
\\ No newline at end of file
@@ -10,2 +11,1 @@
-gone
 kept";

    #[test]
    fn numbers_lines_per_side() {
        let hunks = parse_patch(PATCH);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header, "@@ -1,4 +1,5 @@ fn main() {");

        let numbers: Vec<_> = hunks[0].lines.iter().map(|l| (l.old, l.new)).collect();
        assert_eq!(
            numbers,
            [
                (Some(1), Some(1)),
                (Some(2), None),
                (None, Some(2)),
                (None, Some(3)),
                (Some(3), Some(4)),
            ]
        );
        assert!(hunks[0].lines[4].no_newline);
        assert_eq!(hunks[1].lines[0].old, Some(10));
        assert_eq!(hunks[1].lines[1].new, Some(11));
    }

    #[test]
    fn pairs_changes_side_by_side() {
        let hunks = parse_patch(PATCH);
        let rows = split_rows(&hunks[0].lines);
        let as_pairs: Vec<_> = rows.iter().map(|r| (r.old, r.new)).collect();
        assert_eq!(
            as_pairs,
            [
                (Some(0), Some(0)),
                (Some(1), Some(2)),
                (None, Some(3)),
                (Some(4), Some(4)),
            ]
        );
    }

    #[test]
    fn emphasises_only_the_changed_word() {
        let hunks = parse_patch(PATCH);
        let lines = &hunks[0].lines;
        // "let b = 2;" vs "let b = 3;": only the digit differs.
        assert_eq!(lines[1].emphasis, [(8, 9)]);
        assert_eq!(lines[2].emphasis, [(8, 9)]);
        // An added line with no counterpart gets no word emphasis.
        assert!(lines[3].emphasis.is_empty());
    }

    #[test]
    fn skips_emphasis_for_unrelated_lines() {
        assert_eq!(word_diff("fn alpha(x: u32)", "// totally different"), None);
    }

    #[test]
    fn handles_multibyte_text() {
        let (old, new) = word_diff("naïve café", "naïve thé").unwrap();
        assert_eq!(old, [(7, 12)]);
        assert_eq!(new, [(7, 11)]);
    }

    #[test]
    fn finds_first_changed_line() {
        let file = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            additions: 2,
            deletions: 1,
            hunks: parse_patch(PATCH),
            patch_missing: false,
        };
        assert_eq!(file.first_changed_line(), Some(2));
    }

    #[test]
    fn maps_changes_onto_head_lines() {
        let file = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            additions: 2,
            deletions: 2,
            hunks: parse_patch(PATCH),
            patch_missing: false,
        };
        let changes = file.head_changes();
        assert_eq!(changes.added, [2, 3].into());
        // "-let b = 2;" sat where head line 2 is now; "-gone" before line 11.
        assert_eq!(changes.deleted_before, [2, 11].into());

        assert_eq!(
            file.line_ranges(),
            (vec![(1, 3), (10, 11)], vec![(1, 4), (11, 11)])
        );

        let hunks = &file.hunks;
        assert_eq!(hunks[0].head_line(0), Some(1));
        assert_eq!(hunks[0].head_line(1), Some(2));
        assert_eq!(hunks[1].head_line(0), Some(11));
    }

    #[test]
    fn anchors_comments_the_way_github_does() {
        let file = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            additions: 2,
            deletions: 2,
            hunks: parse_patch(PATCH),
            patch_missing: false,
        };
        let at = |side, line| Anchor { side, line };
        let hunk = &file.hunks[0];
        // Unified: a removed line is the base's, any other the head's.
        assert_eq!(hunk.anchor(0, None), Some(at(Side::New, 1)));
        assert_eq!(hunk.anchor(1, None), Some(at(Side::Old, 2)));
        assert_eq!(hunk.anchor(2, None), Some(at(Side::New, 2)));
        // Split: the column says, and an added line has no base side.
        assert_eq!(hunk.anchor(0, Some(Side::Old)), Some(at(Side::Old, 1)));
        assert_eq!(hunk.anchor(2, Some(Side::Old)), None);

        assert_eq!(file.locate(at(Side::Old, 2)), Some((0, 1)));
        assert_eq!(file.locate(at(Side::New, 4)), Some((0, 4)));
        assert_eq!(file.locate(at(Side::Old, 10)), Some((1, 0)));
        assert_eq!(file.locate(at(Side::New, 30)), None);

        // The removed line has no place in the head's version.
        assert_eq!(hunk.head_text(0, 3), "let a = 1;\nlet b = 3;\nlet c = 4;");
        assert_eq!(hunk.head_text(3, 9), "");
    }
}
