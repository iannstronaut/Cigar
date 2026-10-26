//! Python parser (docs/07 sections 4.4, 8.5).

use tree_sitter::{Node, Tree};

use crate::domain::{EdgeRelation, Language, RepoPath, SymbolKind};
use crate::traits::{LanguageParser, ParseError, ParsedEdge, ParsedFile, ParsedSymbol};

use super::common::{
    collapse, end_line, node_name, node_text, parse_error, signature_before_body, start_line,
    token_stream, walk, ParserCell, MAX_SYMBOLS_PER_FILE,
};

/// Python parser.
pub struct PythonParser {
    cell: ParserCell,
}

impl PythonParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cell: ParserCell::new(tree_sitter_python::LANGUAGE.into()),
        }
    }

    fn tree<'a>(&self, path: &RepoPath, source: &'a str) -> Result<Tree, ParseError> {
        self.cell
            .parse(source)
            .ok_or_else(|| parse_error(path, "tree-sitter gagal memparse"))
    }
}

impl Default for PythonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for PythonParser {
    fn language(&self) -> Language {
        Language::Python
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py"]
    }

    fn parse(&self, path: &RepoPath, source: &str) -> Result<ParsedFile, ParseError> {
        let tree = self.tree(path, source)?;
        let root = tree.root_node();
        let mut out = ParsedFile::default();
        out.imports = collect_imports(root, source);

        let mut symbols = Vec::new();
        let mut edges = Vec::new();
        collect_items(
            root,
            source,
            &mut Vec::new(),
            None,
            &mut symbols,
            &mut edges,
        );
        if symbols.len() > MAX_SYMBOLS_PER_FILE {
            symbols.truncate(MAX_SYMBOLS_PER_FILE);
        }
        out.symbols = symbols;
        out.edges = dedupe(edges);
        Ok(out)
    }
}

fn collect_items(
    node: Node<'_>,
    source: &str,
    scope: &mut Vec<String>,
    class: Option<&str>,
    symbols: &mut Vec<ParsedSymbol>,
    edges: &mut Vec<ParsedEdge>,
) {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for child in children {
        // A class body is a `block` node; descend so methods are found.
        if child.kind() == "block" && class.is_some() {
            collect_items(child, source, scope, class, symbols, edges);
            continue;
        }
        match child.kind() {
            "function_definition" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = qualify(scope, class, name);
                let kind = if class.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                symbols.push(build_symbol(child, source, name, &qualified, kind, "body"));
                collect_edges(child, source, &qualified, edges);
                let mut nested = scope.clone();
                nested.push(name.to_owned());
                collect_items(child, source, &mut nested, None, symbols, edges);
            }
            "decorated_definition" => {
                // The docstring and signature belong to the inner definition,
                // but decorators are part of the reader-visible header, so keep
                // the whole decorated node as the code snippet.
                let Some(inner) = child.child_by_field_name("definition") else {
                    continue;
                };
                if !matches!(
                    inner.kind(),
                    "function_definition" | "class_definition" | "async_function_definition"
                ) {
                    continue;
                }
                let Some(name) = node_name(inner, source) else {
                    continue;
                };
                let qualified = qualify(scope, class, name);
                let kind = match inner.kind() {
                    "class_definition" => SymbolKind::Class,
                    _ if class.is_some() => SymbolKind::Method,
                    _ => SymbolKind::Function,
                };
                let mut symbol = build_symbol(
                    inner,
                    source,
                    name,
                    &qualified,
                    kind,
                    if kind == SymbolKind::Class {
                        ""
                    } else {
                        "body"
                    },
                );
                symbol.code = truncate_chars(node_text(child, source), 12_000);
                symbol.signature =
                    truncate_chars(&collapse(&decorated_signature(child, inner, source)), 500);
                symbol.start_line = start_line(child);
                symbols.push(symbol);
                if kind == SymbolKind::Class {
                    collect_items(inner, source, scope, Some(name), symbols, edges);
                } else {
                    collect_edges(inner, source, &qualified, edges);
                    let mut nested = scope.clone();
                    nested.push(name.to_owned());
                    collect_items(inner, source, &mut nested, None, symbols, edges);
                }
            }
            "class_definition" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = qualify(scope, class, name);
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    SymbolKind::Class,
                    "",
                ));
                collect_base_classes(child, source, &qualified, edges);
                collect_items(child, source, scope, Some(name), symbols, edges);
            }
            "async_function_definition" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = qualify(scope, class, name);
                let kind = if class.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                symbols.push(build_symbol(child, source, name, &qualified, kind, "body"));
                collect_edges(child, source, &qualified, edges);
            }
            _ => {}
        }
    }
}

fn qualify(scope: &[String], class: Option<&str>, name: &str) -> String {
    match class {
        Some(class) => format!("{class}::{name}"),
        None => {
            if scope.is_empty() {
                name.to_owned()
            } else {
                format!("{}::{name}", scope.join("::"))
            }
        }
    }
}

fn build_symbol(
    node: Node<'_>,
    source: &str,
    name: &str,
    qualified_name: &str,
    kind: SymbolKind,
    body_field: &str,
) -> ParsedSymbol {
    let signature = if body_field.is_empty() {
        collapse(node_text(node, source))
    } else {
        signature_before_body(node, source, body_field)
    };
    let code = truncate_chars(node_text(node, source), 12_000);
    let docstring = docstring_of(node, source).map(|doc| truncate_chars(&doc, 2_000));
    let body = if body_field.is_empty() {
        token_stream(node, source)
    } else {
        node.child_by_field_name(body_field).map_or_else(
            || token_stream(node, source),
            |body| token_stream(body, source),
        )
    };
    ParsedSymbol {
        name: name.to_owned(),
        qualified_name: qualified_name.to_owned(),
        kind,
        signature: truncate_chars(&signature, 500),
        docstring,
        code,
        start_line: start_line(node),
        end_line: end_line(node),
        body_stream: body,
    }
}

/// Returns the first string literal inside the body as the docstring.
fn docstring_of(node: Node<'_>, source: &str) -> Option<String> {
    let body = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("block"))?;
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let mut inner = first.walk();
    let literal = first.named_children(&mut inner).next()?;
    if literal.kind() != "string" {
        return None;
    }
    let raw = node_text(literal, source);
    Some(unescape_string_literal(raw))
}

fn unescape_string_literal(raw: &str) -> String {
    let trimmed = raw.trim();
    // Handle prefixes (r, b, f, u) and both quote styles, including triples.
    let mut body = trimmed;
    for prefix in ["rb", "br", "r", "b", "f", "u"] {
        if let Some(rest) = body.strip_prefix(prefix) {
            body = rest;
            break;
        }
    }
    for quote in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(rest) = body.strip_prefix(quote) {
            body = rest;
            break;
        }
    }
    for quote in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(rest) = body.strip_suffix(quote) {
            body = rest;
            break;
        }
    }
    body.replace("\\n", "\n")
        .replace("\\\"", "\"")
        .replace("\\'", "'")
}

fn decorated_signature(decorated: Node<'_>, inner: Node<'_>, source: &str) -> String {
    let end = inner
        .child_by_field_name("body")
        .map_or(inner.end_byte(), |body| body.start_byte());
    source
        .get(decorated.start_byte()..end)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn collect_base_classes(
    class: Node<'_>,
    source: &str,
    qualified_name: &str,
    edges: &mut Vec<ParsedEdge>,
) {
    let Some(bases) = class.child_by_field_name("superclasses") else {
        return;
    };
    let mut cursor = bases.walk();
    for child in bases.named_children(&mut cursor) {
        if child.kind() != "identifier" && child.kind() != "attribute" {
            continue;
        }
        let text = node_text(child, source);
        let name = text.rsplit('.').next().unwrap_or(text);
        if !name.is_empty() {
            edges.push(ParsedEdge {
                source_qualified_name: qualified_name.to_owned(),
                target_name: name.to_owned(),
                relation: EdgeRelation::Implements,
            });
        }
    }
}

fn collect_imports(root: Node<'_>, source: &str) -> String {
    let mut imports = Vec::new();
    walk(root, &mut |node| {
        if matches!(node.kind(), "import_statement" | "import_from_statement") {
            imports.push(collapse(node_text(node, source)));
        }
    });
    truncate_chars(&imports.join("\n"), 4_000)
}

fn collect_edges(node: Node<'_>, source: &str, qualified_name: &str, edges: &mut Vec<ParsedEdge>) {
    let mut calls = 0_usize;
    let mut types = 0_usize;
    let mut visit = |child: Node<'_>| match child.kind() {
        "call" => {
            if calls >= 200 {
                return;
            }
            if let Some(function) = child.child_by_field_name("function") {
                if let Some(target) = callee(function, source) {
                    if is_builtin(&target) {
                        return;
                    }
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: target,
                        relation: EdgeRelation::Calls,
                    });
                    calls += 1;
                }
            }
        }
        "type" if types < 100 => {
            let text = node_text(child, source);
            let name = text.trim().rsplit('.').next().unwrap_or(text).trim();
            if !name.is_empty() {
                edges.push(ParsedEdge {
                    source_qualified_name: qualified_name.to_owned(),
                    target_name: name.to_owned(),
                    relation: EdgeRelation::UsesType,
                });
                types += 1;
            }
        }
        "class_definition" => {
            if types >= 100 {
                return;
            }
            if let Some(name) = node_name(child, source) {
                edges.push(ParsedEdge {
                    source_qualified_name: qualified_name.to_owned(),
                    target_name: name.to_owned(),
                    relation: EdgeRelation::UsesType,
                });
                types += 1;
            }
        }
        _ => {}
    };
    walk(node, &mut visit);
}

fn callee(function: Node<'_>, source: &str) -> Option<String> {
    match function.kind() {
        "identifier" => source.get(function.byte_range()).map(str::to_owned),
        "attribute" => function
            .child_by_field_name("attribute")
            .and_then(|attribute| source.get(attribute.byte_range()))
            .map(str::to_owned),
        _ => {
            let text = node_text(function, source);
            text.rsplit('.').next().map(str::to_owned)
        }
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "len"
            | "str"
            | "int"
            | "float"
            | "bool"
            | "list"
            | "dict"
            | "set"
            | "tuple"
            | "range"
            | "enumerate"
            | "zip"
            | "open"
            | "isinstance"
            | "getattr"
            | "setattr"
            | "hasattr"
            | "super"
            | "sorted"
            | "min"
            | "max"
            | "sum"
            | "any"
            | "all"
            | "abs"
            | "repr"
            | "type"
    )
}

fn dedupe(edges: Vec<ParsedEdge>) -> Vec<ParsedEdge> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(edges.len());
    for edge in edges {
        let key = (
            edge.source_qualified_name.clone(),
            edge.target_name.clone(),
            edge.relation,
        );
        if seen.insert(key) {
            out.push(edge);
        }
    }
    out
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(limit.saturating_sub(1)).collect();
    out.push('~');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> ParsedFile {
        PythonParser::new()
            .parse(&RepoPath::parse("app/auth.py").unwrap(), source)
            .unwrap()
    }

    fn symbol<'a>(file: &'a ParsedFile, qualified: &str) -> &'a ParsedSymbol {
        file.symbols
            .iter()
            .find(|symbol| symbol.qualified_name == qualified)
            .unwrap_or_else(|| {
                panic!(
                    "simbol {qualified} tidak ada; ada {:?}",
                    file.symbols
                        .iter()
                        .map(|s| s.qualified_name.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn function_with_docstring() {
        let file = parse(
            "def authenticate(token):\n    \"\"\"Checks a token.\"\"\"\n    return validate(token)\n",
        );
        let auth = symbol(&file, "authenticate");
        assert_eq!(auth.kind, SymbolKind::Function);
        assert_eq!(auth.docstring.as_deref(), Some("Checks a token."));
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "authenticate"
                && edge.target_name == "validate"
                && edge.relation == EdgeRelation::Calls
        }));
    }

    #[test]
    fn class_methods_are_qualified() {
        let file = parse(
            "class Repo:\n    def save(self):\n        return None\n    def load(self):\n        return self.save()\n",
        );
        assert_eq!(symbol(&file, "Repo").kind, SymbolKind::Class);
        assert_eq!(symbol(&file, "Repo::save").kind, SymbolKind::Method);
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Repo::load" && edge.target_name == "save"
        }));
    }

    #[test]
    fn inheritance_becomes_implements_edge() {
        let file = parse("class Child(Base):\n    pass\n");
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Child"
                && edge.target_name == "Base"
                && edge.relation == EdgeRelation::Implements
        }));
    }

    #[test]
    fn decorators_are_part_of_the_signature() {
        let file = parse("@app.route(\"/health\")\ndef health():\n    return \"ok\"\n");
        let health = symbol(&file, "health");
        assert!(health.signature.contains("@app.route"));
        assert!(health.code.contains("@app.route"));
    }

    #[test]
    fn imports_are_collected() {
        let file = parse("import os\nfrom pathlib import Path\n");
        assert!(file.imports.contains("import os"));
        assert!(file.imports.contains("from pathlib import Path"));
    }

    #[test]
    fn async_functions_are_found() {
        let file = parse("async def fetch(url):\n    return url\n");
        assert_eq!(symbol(&file, "fetch").kind, SymbolKind::Function);
    }

    #[test]
    fn builtins_are_not_call_edges() {
        let file = parse("def log(items):\n    print(len(items))\n");
        assert!(!file
            .edges
            .iter()
            .any(|edge| edge.target_name == "print" || edge.target_name == "len"));
    }
}
