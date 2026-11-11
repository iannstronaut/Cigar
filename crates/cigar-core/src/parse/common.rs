// SPDX-License-Identifier: MPL-2.0

//! Shared tree-sitter helpers: parser construction, byte ranges, line numbers,
//! and identifier extraction.

use std::sync::Mutex;

use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

use crate::domain::{Language, RepoPath};
use crate::traits::{LanguageParser, ParseError, ParsedFile};

/// Registry of parsers keyed by language.
#[derive(Default)]
pub struct ParserRegistry {
    parsers: Vec<Box<dyn LanguageParser>>,
}

impl std::fmt::Debug for ParserRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let languages: Vec<&str> = self.parsers.iter().map(|p| p.language().as_str()).collect();
        f.debug_struct("ParserRegistry")
            .field("languages", &languages)
            .finish()
    }
}

impl ParserRegistry {
    /// A registry with every built-in parser.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut registry = Self::default();
        registry.register(Box::new(super::RustParser::new()));
        registry.register(Box::new(super::TypeScriptParser::new()));
        registry.register(Box::new(super::JavaScriptParser::new()));
        registry.register(Box::new(super::GoParser::new()));
        registry.register(Box::new(super::PythonParser::new()));
        registry
    }

    /// Builder for customized registries (tests, future languages).
    #[must_use]
    pub fn builder() -> ParserRegistryBuilder {
        ParserRegistryBuilder::default()
    }

    /// Registers one parser.
    pub fn register(&mut self, parser: Box<dyn LanguageParser>) {
        self.parsers
            .retain(|existing| existing.language() != parser.language());
        self.parsers.push(parser);
    }

    /// Language that will parse this path, if any.
    #[must_use]
    pub fn for_path(&self, path: &RepoPath) -> Option<Language> {
        let language = Language::from_path(path)?;
        self.parsers
            .iter()
            .any(|parser| parser.language() == language)
            .then_some(language)
    }

    /// Parses `source` using the parser registered for `language`.
    pub fn parse(
        &self,
        language: Language,
        path: &RepoPath,
        source: &str,
    ) -> Result<ParsedFile, ParseError> {
        let parser = self
            .parsers
            .iter()
            .find(|parser| parser.language() == language)
            .ok_or_else(|| ParseError::new(path.as_str(), "tidak ada parser untuk bahasa ini"))?;
        parser.parse(path, source)
    }

    /// Registered languages, for diagnostics.
    #[must_use]
    pub fn languages(&self) -> Vec<Language> {
        self.parsers
            .iter()
            .map(|parser| parser.language())
            .collect()
    }
}

/// Builder for `ParserRegistry`.
#[derive(Default)]
pub struct ParserRegistryBuilder {
    registry: ParserRegistry,
}

impl ParserRegistryBuilder {
    /// Adds the Rust parser.
    #[must_use]
    pub fn rust(mut self) -> Self {
        self.registry.register(Box::new(super::RustParser::new()));
        self
    }

    /// Adds the TypeScript/TSX parser.
    #[must_use]
    pub fn typescript(mut self) -> Self {
        self.registry
            .register(Box::new(super::TypeScriptParser::new()));
        self
    }

    /// Adds the JavaScript parser.
    #[must_use]
    pub fn javascript(mut self) -> Self {
        self.registry
            .register(Box::new(super::JavaScriptParser::new()));
        self
    }

    /// Adds every parser needed by v0.1.
    #[must_use]
    pub fn build(self) -> ParserRegistry {
        self.registry
    }
}

/// A `tree_sitter::Parser` reused across calls on one thread.
pub(crate) struct ParserCell {
    parser: Mutex<Parser>,
}

impl ParserCell {
    pub(crate) fn new(language: TsLanguage) -> Self {
        let mut parser = Parser::new();
        // A grammar mismatch here would be a build-time incompatibility; the
        // ABI is validated by tree-sitter itself, so a failure is fatal config.
        if parser.set_language(&language).is_err() {
            // The caller converts this into `ParseError` per file; keeping the
            // parser alive means every parse fails the same way deterministically.
            return Self {
                parser: Mutex::new(parser),
            };
        }
        Self {
            parser: Mutex::new(parser),
        }
    }

    pub(crate) fn parse(&self, source: &str) -> Option<Tree> {
        let mut parser = self.parser.lock().ok()?;
        parser.parse(source, None)
    }
}

/// Converts a tree-sitter node's 0-based row to a 1-based line number.
#[must_use]
pub(crate) fn start_line(node: Node<'_>) -> u32 {
    node.start_position().row as u32 + 1
}

/// Converts a node's end position to the 1-based inclusive last line.
#[must_use]
pub(crate) fn end_line(node: Node<'_>) -> u32 {
    let point = node.end_position();
    let last_row = if point.column == 0 && point.row > node.start_position().row {
        point.row.saturating_sub(1)
    } else {
        point.row
    };
    last_row as u32 + 1
}

/// Extracts the source text covered by a node.
#[must_use]
pub(crate) fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or_default()
}

/// Reads `name` by field, then falls back to the first identifier child.
#[must_use]
pub(crate) fn node_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    if let Some(name) = node.child_by_field_name("name") {
        return source.get(name.byte_range());
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "identifier" | "type_identifier" | "property_identifier" | "field_identifier"
            )
        })
        .and_then(|child| source.get(child.byte_range()));
    found
}

/// Walks the tree depth-first, invoking `visit` for each named node.
pub(crate) fn walk<'tree>(root: Node<'tree>, visit: &mut dyn FnMut(Node<'tree>)) {
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        visit(node);
        let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

/// Normalized token stream for `body_hash`: identifier/literal/operator values
/// separated by unit separators, whitespace and comments dropped.
pub(crate) fn token_stream(node: Node<'_>, source: &str) -> String {
    let mut tokens: Vec<&str> = Vec::new();
    let mut stack = vec![node];
    let mut cursor = node.walk();
    while let Some(current) = stack.pop() {
        let kind = current.kind();
        // Skip comment subtrees entirely: doc comments are not code, and their
        // delimiter tokens must not leak into the hash (docs/07 section 7.1).
        if is_comment_kind(kind) {
            continue;
        }
        let children: Vec<Node<'_>> = current.children(&mut cursor).collect();
        if children.is_empty() {
            if let Some(text) = source.get(current.byte_range()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    tokens.push(trimmed);
                }
            }
            continue;
        }
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    tokens.join("\u{1f}")
}

pub(crate) fn is_comment_kind(kind: &str) -> bool {
    matches!(kind, "comment" | "line_comment" | "block_comment")
}

/// Collapses whitespace runs and trims, used for signatures and imports.
#[must_use]
pub(crate) fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Signature for a node: the header up to the body, whitespace-collapsed.
#[must_use]
pub(crate) fn signature_before_body<'a>(
    node: Node<'_>,
    source: &'a str,
    body_field: &str,
) -> String {
    let end = node
        .child_by_field_name(body_field)
        .map_or(node.end_byte(), |body| body.start_byte());
    let text = source.get(node.start_byte()..end).unwrap_or_default();
    let text = text.trim_end().trim_end_matches('{').trim_end();
    collapse(text)
}

/// Extracts a doc comment immediately preceding a node, if any. When the node
/// is wrapped in an `export_statement`, the export's own preceding siblings are
/// checked first, because JSDoc attaches to the statement, not the declaration.
#[must_use]
pub(crate) fn preceding_doc(node: Node<'_>, source: &str, kinds: &[&str]) -> Option<String> {
    let anchor = if node
        .parent()
        .is_some_and(|parent| parent.kind() == "export_statement")
    {
        node.parent().unwrap_or(node)
    } else {
        node
    };
    let mut current = anchor;
    let mut collected: Vec<String> = Vec::new();
    while let Some(previous) = current.prev_named_sibling() {
        if !kinds.contains(&previous.kind()) {
            break;
        }
        let text = node_text(previous, source);
        collected.push(text.to_owned());
        current = previous;
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let joined = collected.join("\n");
    let normalized = crate::hash::normalize_doc(&joined);
    (!normalized.is_empty()).then_some(normalized)
}

/// Extracts a type name from an `implements_clause` or `extends_clause`.
#[must_use]
pub(crate) fn clause_type_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "type_identifier"
            | "identifier"
            | "generic_type"
            | "nested_type_identifier"
            | "qualified_name" => {
                let text = node_text(child, source);
                let base = text
                    .split('<')
                    .next()
                    .unwrap_or(text)
                    .rsplit('.')
                    .next()
                    .unwrap_or(text)
                    .trim();
                if !base.is_empty() {
                    out.push(base.to_owned());
                }
            }
            _ => {}
        }
    }
    out
}

/// Formats a parse failure with a stable message.
#[must_use]
pub(crate) fn parse_error(path: &RepoPath, message: impl AsRef<str>) -> ParseError {
    ParseError::new(path.as_str(), message.as_ref())
}

/// Cap on symbol count per file, to keep one pathological file from exhausting
/// memory (docs/07 section 10).
pub(crate) const MAX_SYMBOLS_PER_FILE: usize = 50_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_numbers_are_one_based() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn a() {}\nfn b() {}\n", None).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let functions: Vec<Node<'_>> = root
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "function_item")
            .collect();
        assert_eq!(start_line(functions[0]), 1);
        assert_eq!(end_line(functions[0]), 1);
        assert_eq!(start_line(functions[1]), 2);
    }

    #[test]
    fn token_stream_skips_comments() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let source = "fn a() { let x = 1; // comment\n }";
        let tree = parser.parse(source, None).unwrap();
        let stream = token_stream(tree.root_node(), source);
        assert!(!stream.contains("comment"), "{stream}");
        assert!(stream.contains('x'));
    }
}
