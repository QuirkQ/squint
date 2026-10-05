//! Syntax highlighting with tree-sitter, the parser Zed uses, for the Code
//! tab. Languages are picked by file name; anything else is shown plain.

use std::sync::OnceLock;

use shared::code::HIGHLIGHTS;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Bash,
    Css,
    Erb,
    Go,
    Html,
    Javascript,
    Json,
    Markdown,
    MarkdownInline,
    Python,
    Ruby,
    Rust,
    Toml,
    Tsx,
    Typescript,
    Yaml,
}

const LANGS: usize = 16;

impl Lang {
    fn for_path(path: &str) -> Option<Lang> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let by_name = match name {
            "Gemfile" | "Rakefile" | "Guardfile" | "Capfile" | "Brewfile" | "Podfile"
            | "Vagrantfile" | "Fastfile" | "Dangerfile" | "config.ru" | ".irbrc" | ".pryrc" => {
                Some(Lang::Ruby)
            }
            "Cargo.lock" | "Pipfile" | "poetry.lock" | "uv.lock" => Some(Lang::Toml),
            ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".envrc" => Some(Lang::Bash),
            _ => None,
        };
        if by_name.is_some() {
            return by_name;
        }
        let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match extension.as_str() {
            "rb" | "rake" | "gemspec" | "ru" | "thor" | "jbuilder" | "builder" | "podspec" => {
                Lang::Ruby
            }
            "erb" | "rhtml" => Lang::Erb,
            "rs" => Lang::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Lang::Javascript,
            "ts" | "mts" | "cts" => Lang::Typescript,
            "tsx" => Lang::Tsx,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "yml" | "yaml" => Lang::Yaml,
            "json" | "jsonc" | "json5" | "webmanifest" => Lang::Json,
            "toml" => Lang::Toml,
            "sh" | "bash" | "zsh" | "bats" => Lang::Bash,
            "css" => Lang::Css,
            "html" | "htm" | "svg" | "xml" => Lang::Html,
            "md" | "markdown" => Lang::Markdown,
            _ => return None,
        })
    }

    /// Languages embedded in another: Markdown code fences, `<script>` in
    /// HTML, the Ruby and HTML of an ERB template.
    fn for_injection(name: &str) -> Option<Lang> {
        Some(match name.to_ascii_lowercase().as_str() {
            "ruby" | "rb" => Lang::Ruby,
            "erb" => Lang::Erb,
            "rust" | "rs" => Lang::Rust,
            "javascript" | "js" | "jsx" => Lang::Javascript,
            "typescript" | "ts" => Lang::Typescript,
            "tsx" => Lang::Tsx,
            "python" | "py" => Lang::Python,
            "go" => Lang::Go,
            "yaml" | "yml" => Lang::Yaml,
            "json" => Lang::Json,
            "toml" => Lang::Toml,
            "bash" | "sh" | "shell" | "zsh" | "console" => Lang::Bash,
            "css" => Lang::Css,
            "html" => Lang::Html,
            "markdown" | "md" => Lang::Markdown,
            "markdown_inline" => Lang::MarkdownInline,
            _ => return None,
        })
    }

    /// Built on first use: compiling a grammar's queries takes a moment.
    fn config(self) -> Option<&'static HighlightConfiguration> {
        static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; LANGS] =
            [const { OnceLock::new() }; LANGS];
        CONFIGS[self as usize].get_or_init(|| self.build()).as_ref()
    }

    fn build(self) -> Option<HighlightConfiguration> {
        use tree_sitter_typescript as ts;
        let js_highlights = tree_sitter_javascript::HIGHLIGHT_QUERY;
        let jsx_highlights = tree_sitter_javascript::JSX_HIGHLIGHT_QUERY;
        // TypeScript's queries only add to JavaScript's. When patterns match
        // the same node the later one wins, so the additions go last.
        let typescript = format!("{js_highlights}\n{}", ts::HIGHLIGHTS_QUERY);
        let tsx = format!(
            "{js_highlights}\n{jsx_highlights}\n{}",
            ts::HIGHLIGHTS_QUERY
        );
        let javascript = format!("{js_highlights}\n{jsx_highlights}");
        let markdown_injections = markdown_block_injections();
        let ts_locals = format!(
            "{}\n{}",
            ts::LOCALS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY
        );

        let (language, name, highlights, injections, locals) = match self {
            Lang::Bash => (
                tree_sitter_bash::LANGUAGE,
                "bash",
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Lang::Css => (
                tree_sitter_css::LANGUAGE,
                "css",
                tree_sitter_css::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Lang::Erb => (
                tree_sitter_embedded_template::LANGUAGE,
                "erb",
                tree_sitter_embedded_template::HIGHLIGHTS_QUERY,
                tree_sitter_embedded_template::INJECTIONS_ERB_QUERY,
                "",
            ),
            Lang::Go => (
                tree_sitter_go::LANGUAGE,
                "go",
                tree_sitter_go::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Lang::Html => (
                tree_sitter_html::LANGUAGE,
                "html",
                tree_sitter_html::HIGHLIGHTS_QUERY,
                tree_sitter_html::INJECTIONS_QUERY,
                "",
            ),
            Lang::Javascript => (
                tree_sitter_javascript::LANGUAGE,
                "javascript",
                javascript.as_str(),
                tree_sitter_javascript::INJECTIONS_QUERY,
                tree_sitter_javascript::LOCALS_QUERY,
            ),
            Lang::Json => (
                tree_sitter_json::LANGUAGE,
                "json",
                tree_sitter_json::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Lang::Markdown => (
                tree_sitter_md::LANGUAGE,
                "markdown",
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
                markdown_injections.as_str(),
                "",
            ),
            Lang::MarkdownInline => (
                tree_sitter_md::INLINE_LANGUAGE,
                "markdown_inline",
                tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
                tree_sitter_md::INJECTION_QUERY_INLINE,
                "",
            ),
            Lang::Python => (
                tree_sitter_python::LANGUAGE,
                "python",
                tree_sitter_python::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Lang::Ruby => (
                tree_sitter_ruby::LANGUAGE,
                "ruby",
                tree_sitter_ruby::HIGHLIGHTS_QUERY,
                "",
                tree_sitter_ruby::LOCALS_QUERY,
            ),
            Lang::Rust => (
                tree_sitter_rust::LANGUAGE,
                "rust",
                tree_sitter_rust::HIGHLIGHTS_QUERY,
                tree_sitter_rust::INJECTIONS_QUERY,
                "",
            ),
            Lang::Toml => (
                tree_sitter_toml_ng::LANGUAGE,
                "toml",
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Lang::Tsx => (
                ts::LANGUAGE_TSX,
                "tsx",
                tsx.as_str(),
                "",
                ts_locals.as_str(),
            ),
            Lang::Typescript => (
                ts::LANGUAGE_TYPESCRIPT,
                "typescript",
                typescript.as_str(),
                "",
                ts_locals.as_str(),
            ),
            Lang::Yaml => (
                tree_sitter_yaml::LANGUAGE,
                "yaml",
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
        };
        let mut config =
            HighlightConfiguration::new(language.into(), name, highlights, injections, locals)
                .map_err(|error| eprintln!("highlighting {name}: {error}"))
                .ok()?;
        let names: Vec<&str> = HIGHLIGHTS.iter().map(|(name, _)| *name).collect();
        config.configure(&names);
        Some(config)
    }
}

/// The block grammar's injections, minus the one for its inline text: that
/// one loses most of its highlights inside tree-sitter-highlight (code
/// spans, emphasis, URLs), so [`markdown_inline`] does it instead.
fn markdown_block_injections() -> String {
    const INLINE: &str =
        "((inline) @injection.content\n  (#set! injection.language \"markdown_inline\"))";
    let query = tree_sitter_md::INJECTION_QUERY_BLOCK;
    debug_assert!(
        query.contains(INLINE),
        "tree-sitter-md's injections changed"
    );
    query.replace(INLINE, "")
}

/// Highlights for `text` as flat `[start, end, kind]` byte triples, where
/// `kind` indexes [`HIGHLIGHTS`]. No span crosses a line break. Empty for
/// languages it doesn't know.
pub fn highlight(path: &str, text: &str) -> Vec<u32> {
    match Lang::for_path(path) {
        Some(Lang::Markdown) => merge(highlight_as(Lang::Markdown, text), markdown_inline(text)),
        Some(lang) => highlight_as(lang, text),
        None => Vec::new(),
    }
}

/// Markdown's inline text (code spans, emphasis, links) outside headings,
/// highlighted one run at a time.
fn markdown_inline(text: &str) -> Vec<u32> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut cursor = tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        let in_heading = node
            .parent()
            .is_some_and(|p| p.kind().ends_with("_heading"));
        if visit && node.kind() == "inline" && !in_heading {
            let offset = node.start_byte() as u32;
            let run = highlight_as(Lang::MarkdownInline, &text[node.byte_range()]);
            spans.extend(
                run.as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|&[start, end, kind]| [start + offset, end + offset, kind]),
            );
        }
        if visit && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        visit = false;
    }
    spans
}

/// Both span lists in one, in order; where they overlap, the earlier span
/// keeps its bytes.
fn merge(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    let mut all: Vec<[u32; 3]> = [a.as_chunks::<3>().0, b.as_chunks::<3>().0].concat();
    all.sort_unstable();
    let mut merged = Vec::with_capacity(all.len() * 3);
    let mut end = 0;
    for [start, stop, kind] in all {
        let start = start.max(end);
        if start < stop {
            merged.extend([start, stop, kind]);
            end = stop;
        }
    }
    merged
}

fn highlight_as(lang: Lang, text: &str) -> Vec<u32> {
    let Some(config) = lang.config() else {
        return Vec::new();
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(config, text.as_bytes(), None, None, |name| {
        Lang::for_injection(name).and_then(Lang::config)
    }) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    let mut active: Vec<usize> = Vec::new();
    for event in events {
        match event {
            Ok(HighlightEvent::HighlightStart(highlight)) => active.push(highlight.0),
            Ok(HighlightEvent::HighlightEnd) => {
                active.pop();
            }
            Ok(HighlightEvent::Source { start, end }) => {
                if let Some(&kind) = active.last() {
                    push_span(&mut spans, text, start, end, kind as u32);
                }
            }
            Err(_) => return Vec::new(),
        }
    }
    spans
}

/// Adds `start..end` as spans cut at line breaks, joining it to the previous
/// span when they touch and match.
fn push_span(spans: &mut Vec<u32>, text: &str, start: usize, end: usize, kind: u32) {
    let mut from = start;
    for (i, _) in text[start..end].match_indices('\n') {
        push_piece(spans, from, start + i, kind);
        from = start + i + 1;
    }
    push_piece(spans, from, end, kind);
}

fn push_piece(spans: &mut Vec<u32>, start: usize, end: usize, kind: u32) {
    if start >= end {
        return;
    }
    let (start, end) = (start as u32, end as u32);
    if let [.., last_end, last_kind] = spans.as_mut_slice()
        && *last_end == start
        && *last_kind == kind
    {
        *last_end = end;
        return;
    }
    spans.extend([start, end, kind]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(text, class)` for each span, to keep assertions readable.
    fn painted(path: &str, text: &str) -> Vec<(String, &'static str)> {
        highlight(path, text)
            .chunks(3)
            .map(|span| {
                let (start, end, kind) = (span[0] as usize, span[1] as usize, span[2] as usize);
                (text[start..end].to_string(), HIGHLIGHTS[kind].1)
            })
            .collect()
    }

    fn class_of(spans: &[(String, &'static str)], text: &str) -> Option<&'static str> {
        spans.iter().find(|(t, _)| t == text).map(|(_, c)| *c)
    }

    #[test]
    fn highlights_ruby() {
        let source = "# frozen_string_literal: true\nmodule ShadowOidc\n  METRIC = \"shadow.count\"\n\n  def self.enabled?(x = :on)\n    config.enabled\n  end\nend\n";
        let spans = painted("app/services/shadow_oidc.rb", source);
        assert_eq!(
            class_of(&spans, "# frozen_string_literal: true"),
            Some("hl-comment")
        );
        assert_eq!(class_of(&spans, "module"), Some("hl-keyword"));
        assert_eq!(class_of(&spans, "def"), Some("hl-keyword"));
        assert_eq!(class_of(&spans, "ShadowOidc"), Some("hl-type"));
        assert_eq!(class_of(&spans, "enabled?"), Some("hl-function"));
        assert!(
            spans
                .iter()
                .any(|(t, c)| t.contains("shadow.count") && *c == "hl-string")
        );
        assert_eq!(class_of(&spans, ":on"), Some("hl-symbol"));
    }

    #[test]
    fn spans_never_cross_lines() {
        let source = "x = <<~SQL\n  select 1\n  from t\nSQL\n/* a\nb */";
        for path in ["a.rb", "a.css"] {
            let spans = highlight(path, source);
            for span in spans.chunks(3) {
                let piece = &source[span[0] as usize..span[1] as usize];
                assert!(!piece.contains('\n'), "{path}: {piece:?}");
            }
        }
        assert!(!highlight("a.rb", source).is_empty());
    }

    #[test]
    fn highlights_injected_languages() {
        let erb = "<div class=\"x\"><%= link_to user.name, :root %></div>\n";
        let spans = painted("app/views/a.html.erb", erb);
        assert_eq!(class_of(&spans, "div"), Some("hl-tag"));
        assert_eq!(class_of(&spans, ":root"), Some("hl-symbol"));

        let markdown =
            "# Title\n\nSome `code` and *em* [x](http://a.b)\n\n```ruby\ndef x = 1\n```\n";
        let spans = painted("README.md", markdown);
        assert_eq!(class_of(&spans, "def"), Some("hl-keyword"));
        assert_eq!(class_of(&spans, "Title"), Some("hl-title"));
        assert_eq!(class_of(&spans, "`code`"), Some("hl-string"));
        assert_eq!(class_of(&spans, "*em*"), Some("hl-emphasis"));
        assert_eq!(class_of(&spans, "http://a.b"), Some("hl-link"));

        let tsx = "const x: number = <div>{y}</div>;\n";
        let spans = painted("a.tsx", tsx);
        assert_eq!(class_of(&spans, "number"), Some("hl-type"));
        assert_eq!(class_of(&spans, "const"), Some("hl-keyword"));
    }

    #[test]
    fn every_language_compiles() {
        for path in [
            "a.sh", "a.css", "a.erb", "a.go", "a.html", "a.js", "a.json", "a.md", "a.py", "a.rb",
            "a.rs", "a.toml", "a.tsx", "a.ts", "a.yml",
        ] {
            let lang = Lang::for_path(path).unwrap();
            assert!(lang.config().is_some(), "{path}");
        }
        assert!(Lang::MarkdownInline.config().is_some());
        assert_eq!(Lang::for_path("Gemfile"), Some(Lang::Ruby));
        assert_eq!(Lang::for_path("LICENSE"), None);
        assert!(highlight("notes.txt", "def x").is_empty());
    }
}
