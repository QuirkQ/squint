//! The Code tab's types, and the pure helpers behind its file tree, ⌘P and
//! ⌘-click.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Every file at a pull request's head commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeTree {
    pub sha: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FileBody {
    Text(String),
    Binary,
    /// Over the size or line limit; Zed shows it instead.
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub sha: String,
    pub size: u64,
    pub body: FileBody,
    /// Syntax highlighting as flat `[start, end, kind]` byte triples of the
    /// text, `kind` indexing [`HIGHLIGHTS`]. No span crosses a line break.
    #[serde(default)]
    pub highlights: Vec<u32>,
}

/// The two commits a pull request's diff compares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffShas {
    pub head: String,
    /// The merge base with the base branch, as GitHub diffs against it.
    pub base: String,
}

/// Syntax highlighting for some lines of a file: each line number with its
/// flat `[start, end, kind]` triples, relative to the start of the line.
pub type LineHighlights = Vec<(u32, Vec<u32>)>;

/// The tree-sitter capture names the highlighter recognises, and the CSS
/// class each is shown with. A capture without an entry of its own uses its
/// longest listed prefix (`function.method.builtin` → `function.method`);
/// plain variables, operators and most punctuation stay unstyled.
pub const HIGHLIGHTS: &[(&str, &str)] = &[
    ("attribute", "hl-property"),
    ("boolean", "hl-number"),
    ("comment", "hl-comment"),
    ("constant", "hl-type"),
    ("constant.builtin", "hl-number"),
    ("constructor", "hl-type"),
    ("embedded", "hl-embedded"),
    ("escape", "hl-escape"),
    ("function", "hl-function"),
    ("keyword", "hl-keyword"),
    ("label", "hl-property"),
    ("namespace", "hl-type"),
    ("number", "hl-number"),
    ("property", "hl-property"),
    ("punctuation.special", "hl-embedded"),
    ("string", "hl-string"),
    ("string.escape", "hl-escape"),
    ("string.special", "hl-escape"),
    ("string.special.key", "hl-property"),
    ("string.special.symbol", "hl-symbol"),
    ("tag", "hl-tag"),
    ("text.emphasis", "hl-emphasis"),
    ("text.literal", "hl-string"),
    ("text.reference", "hl-function"),
    ("text.strong", "hl-strong"),
    ("text.title", "hl-title"),
    ("text.uri", "hl-link"),
    ("type", "hl-type"),
    ("variable.builtin", "hl-builtin"),
    ("variable.parameter", "hl-parameter"),
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepQuery {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Comma-separated globs such as `*.rb, app/**, !spec/**`.
    pub paths: String,
    /// Look `pattern` up as a symbol: its definitions, then every use.
    pub symbol: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: u32,
    /// The line's text, shortened around the match when very long.
    pub text: String,
    /// Byte ranges of `text` that matched.
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GrepResult {
    /// Only for symbol searches.
    pub definitions: Vec<GrepMatch>,
    pub matches: Vec<GrepMatch>,
    /// More matched than fit; only the first ones are listed.
    pub truncated: bool,
}

/// A file or directory in the explorer. Directories holding nothing but one
/// other directory are folded into it (`src/main/java`), as Zed does.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    /// What the row shows: a file name, or `a/b/c` for folded directories.
    pub name: String,
    pub path: String,
    /// `None` for files. Shared, so handing a directory to the UI is cheap.
    pub children: Option<Arc<[TreeNode]>>,
}

impl TreeNode {
    pub fn is_dir(&self) -> bool {
        self.children.is_some()
    }
}

/// Directories first, then files, each sorted the way Finder and Zed sort them.
pub fn build_tree(paths: &[String]) -> Arc<[TreeNode]> {
    #[derive(Default)]
    struct Dir<'a> {
        dirs: std::collections::BTreeMap<&'a str, Dir<'a>>,
        files: Vec<&'a str>,
    }
    let mut root = Dir::default();
    for path in paths {
        let mut dir = &mut root;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                dir = dir.dirs.entry(part).or_default();
            } else {
                dir.files.push(part);
            }
        }
    }

    fn sort_key(name: &str) -> (String, &str) {
        (name.to_lowercase(), name)
    }
    fn convert(dir: Dir, prefix: &str) -> Arc<[TreeNode]> {
        let join = |name: &str| {
            if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            }
        };
        let mut dirs: Vec<(&str, Dir)> = dir.dirs.into_iter().collect();
        dirs.sort_by(|a, b| sort_key(a.0).cmp(&sort_key(b.0)));
        let mut files = dir.files;
        files.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

        let mut nodes = Vec::with_capacity(dirs.len() + files.len());
        for (name, mut sub) in dirs {
            let mut name = name.to_string();
            let mut path = join(&name);
            while sub.files.is_empty() && sub.dirs.len() == 1 {
                let (child, grandchild) = sub.dirs.pop_first().unwrap();
                name = format!("{name}/{child}");
                path = format!("{path}/{child}");
                sub = grandchild;
            }
            let children = convert(sub, &path);
            nodes.push(TreeNode {
                name,
                path,
                children: Some(children),
            });
        }
        nodes.extend(files.into_iter().map(|name| TreeNode {
            name: name.to_string(),
            path: join(name),
            children: None,
        }));
        nodes.into()
    }
    convert(root, "")
}

/// Every directory above `path`: `a/b/c.rs` → `a`, `a/b`.
pub fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// How well `query` fuzzy-matches `path` (higher is better), with the byte
/// offsets of the matched characters, or `None` when it doesn't match. Every
/// query character must appear in order; runs, word starts and the file name
/// score extra, long paths a little less.
pub fn fuzzy_score(query: &str, path: &str) -> Option<(i32, Vec<usize>)> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let chars: Vec<(usize, char)> = path.char_indices().collect();

    // Scanning back from the end finds the latest place a match can start,
    // which favours the file name; scanning forward from there finds the
    // tightest match starting at it.
    let mut qi = query.len();
    let mut start = 0;
    for (index, &(_, c)) in chars.iter().enumerate().rev() {
        if c.to_ascii_lowercase() == query[qi - 1] {
            qi -= 1;
            if qi == 0 {
                start = index;
                break;
            }
        }
    }
    if qi > 0 {
        return None;
    }
    let mut positions = Vec::with_capacity(query.len());
    for &(offset, c) in &chars[start..] {
        if positions.len() < query.len() && c.to_ascii_lowercase() == query[positions.len()] {
            positions.push(offset);
        }
    }

    let mut score = 0i32;
    let mut previous: Option<usize> = None;
    for &offset in &positions {
        let before = path[..offset].chars().next_back();
        let c = path[offset..].chars().next().unwrap_or(' ');
        let word_start = match before {
            None => true,
            Some(b) => {
                matches!(b, '/' | '_' | '-' | '.' | ' ') || (b.is_lowercase() && c.is_uppercase())
            }
        };
        score += 1;
        if word_start {
            score += 8;
        }
        if offset >= name_start {
            score += 4;
        }
        if let Some(prev) = previous
            && path[prev..offset].chars().count() == 1
        {
            score += 6;
        }
        previous = Some(offset);
    }
    if path[name_start..]
        .to_ascii_lowercase()
        .starts_with(&query.iter().collect::<String>())
    {
        score += 20;
    }
    score -= (path.len() / 8) as i32;
    Some((score, positions))
}

/// Characters that make up an identifier, including Ruby's `?`/`!` suffix.
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The identifier around byte `offset` of `text`, for ⌘-click.
pub fn word_at(text: &str, offset: usize) -> Option<&str> {
    let offset = offset.min(text.len());
    if !text.is_char_boundary(offset) {
        return None;
    }
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map_or(offset, |(i, _)| i);
    let mut end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_ident(*c))
        .map_or(text.len(), |(i, _)| offset + i);
    if text[end..].starts_with(['?', '!']) && !text[end + 1..].starts_with('=') {
        end += 1;
    }
    let word = &text[start..end];
    let first = word.chars().next()?;
    (!first.is_ascii_digit()).then_some(word)
}

/// Whether `symbol` can be looked up as one: an identifier, optionally ending
/// in `?` or `!`.
pub fn is_symbol(symbol: &str) -> bool {
    let body = symbol.strip_suffix(['?', '!']).unwrap_or(symbol);
    let mut chars = body.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn builds_a_sorted_tree_with_folded_directories() {
        let tree = build_tree(&paths(&[
            "README.md",
            "src/main/java/App.java",
            "app/models/user.rb",
            "app/models/account.rb",
            "app/Controllers/x.rb",
            "Gemfile",
        ]));
        let names: Vec<_> = tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["app", "src/main/java", "Gemfile", "README.md"]);
        assert_eq!(tree[1].path, "src/main/java");
        assert_eq!(
            tree[1].children.as_ref().unwrap()[0].path,
            "src/main/java/App.java"
        );
        let app: Vec<_> = tree[0]
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(app, ["Controllers", "models"]);
        let models = &tree[0].children.as_ref().unwrap()[1];
        assert_eq!(
            models.children.as_ref().unwrap()[0].path,
            "app/models/account.rb"
        );
        assert!(!models.children.as_ref().unwrap()[0].is_dir());
    }

    #[test]
    fn lists_ancestors() {
        assert_eq!(ancestors("a/b/c.rs").collect::<Vec<_>>(), ["a", "a/b"]);
        assert_eq!(ancestors("c.rs").count(), 0);
    }

    #[test]
    fn fuzzy_prefers_file_names_and_word_starts() {
        assert!(fuzzy_score("xyz", "app/models/user.rb").is_none());
        let (_, at) = fuzzy_score("user", "app/models/user.rb").unwrap();
        assert_eq!(at, [11, 12, 13, 14]);

        let score = |q, p| fuzzy_score(q, p).unwrap().0;
        assert!(score("user", "app/models/user.rb") > score("user", "app/use_real/rb.txt"));
        assert!(score("sesctl", "app/controllers/sessions_controller.rb") > 0);
        assert!(
            score("session", "app/models/session.rb")
                > score("session", "app/controllers/cupido/sessions_controller.rb")
        );
        assert_eq!(fuzzy_score("", "x"), Some((0, Vec::new())));
    }

    #[test]
    fn finds_the_word_under_the_pointer() {
        let line = "  def tenanted?(config) = self.shadow_expiry!";
        assert_eq!(word_at(line, 8), Some("tenanted?"));
        assert_eq!(word_at(line, 6), Some("tenanted?"));
        assert_eq!(word_at(line, 40), Some("shadow_expiry!"));
        assert_eq!(word_at(line, 1), None);
        assert_eq!(word_at("x != y", 0), Some("x"));
        assert_eq!(word_at("42", 1), None);
        assert_eq!(word_at("café_au_lait", 2), Some("café_au_lait"));
    }

    #[test]
    fn recognises_symbols() {
        for good in ["tenanted?", "save!", "_x", "$el", "Foo2"] {
            assert!(is_symbol(good), "{good}");
        }
        for bad in ["", "2x", "a b", "a?b", "a.b", "-x", "?"] {
            assert!(!is_symbol(bad), "{bad}");
        }
    }
}
