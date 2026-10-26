//! Go parser (docs/07 sections 4.3, 8.4).

use tree_sitter::{Node, Tree};

use crate::domain::{EdgeRelation, Language, RepoPath, SymbolKind};
use crate::traits::{LanguageParser, ParseError, ParsedEdge, ParsedFile, ParsedSymbol};

use super::common::{
    collapse, end_line, is_comment_kind, node_name, node_text, parse_error, preceding_doc,
    signature_before_body, start_line, token_stream, walk, ParserCell, MAX_SYMBOLS_PER_FILE,
};

/// Go parser.
pub struct GoParser {
    cell: ParserCell,
}

impl GoParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cell: ParserCell::new(tree_sitter_go::LANGUAGE.into()),
        }
    }

    fn tree<'a>(&self, path: &RepoPath, source: &'a str) -> Result<Tree, ParseError> {
        self.cell
            .parse(source)
            .ok_or_else(|| parse_error(path, "tree-sitter gagal memparse"))
    }
}

impl Default for GoParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for GoParser {
    fn language(&self) -> Language {
        Language::Go
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn parse(&self, path: &RepoPath, source: &str) -> Result<ParsedFile, ParseError> {
        let tree = self.tree(path, source)?;
        let root = tree.root_node();
        let mut out = ParsedFile::default();
        out.imports = collect_imports(root, source);

        let mut symbols = Vec::new();
        let mut edges = Vec::new();
        let mut cursor = root.walk();
        let children: Vec<Node<'_>> = root.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "function_declaration" => {
                    let Some(name) = node_name(child, source) else {
                        continue;
                    };
                    symbols.push(build_symbol(
                        child,
                        source,
                        name,
                        name,
                        SymbolKind::Function,
                        "body",
                    ));
                    collect_edges(child, source, name, &mut edges);
                }
                "method_declaration" => {
                    let receiver = child
                        .child_by_field_name("receiver")
                        .and_then(|receiver| receiver_type(receiver, source));
                    let Some(name) = node_name(child, source) else {
                        continue;
                    };
                    let qualified = receiver
                        .map_or_else(|| name.to_owned(), |receiver| format!("{receiver}::{name}"));
                    symbols.push(build_symbol(
                        child,
                        source,
                        name,
                        &qualified,
                        SymbolKind::Method,
                        "body",
                    ));
                    collect_edges(child, source, &qualified, &mut edges);
                }
                "type_declaration" => {
                    let mut inner = child.walk();
                    let specs: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                    for spec in specs {
                        if spec.kind() != "type_spec" {
                            continue;
                        }
                        let Some(name) = node_name(spec, source) else {
                            continue;
                        };
                        let kind =
                            spec.child_by_field_name("type")
                                .map_or(SymbolKind::Struct, |ty| match ty.kind() {
                                    "struct_type" => SymbolKind::Struct,
                                    "interface_type" => SymbolKind::Interface,
                                    _ => SymbolKind::Struct,
                                });
                        symbols.push(build_symbol(spec, source, name, name, kind, ""));
                        if let Some(ty) = spec.child_by_field_name("type") {
                            if ty.kind() == "interface_type" {
                                collect_method_signatures(ty, source, name, &mut symbols);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if symbols.len() > MAX_SYMBOLS_PER_FILE {
            symbols.truncate(MAX_SYMBOLS_PER_FILE);
        }
        out.symbols = symbols;
        out.edges = dedupe(edges);
        Ok(out)
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
    let docstring =
        preceding_doc(node, source, &["comment"]).map(|doc| truncate_chars(&doc, 2_000));
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

fn collect_method_signatures(
    interface: Node<'_>,
    source: &str,
    interface_name: &str,
    symbols: &mut Vec<ParsedSymbol>,
) {
    let mut cursor = interface.walk();
    for child in interface.named_children(&mut cursor) {
        if child.kind() != "method_elem" && child.kind() != "method_spec" {
            continue;
        }
        let Some(name) = node_name(child, source) else {
            continue;
        };
        symbols.push(ParsedSymbol {
            name: name.to_owned(),
            qualified_name: format!("{interface_name}::{name}"),
            kind: SymbolKind::Method,
            signature: truncate_chars(&collapse(node_text(child, source)), 500),
            docstring: preceding_doc(child, source, &["comment"]),
            code: truncate_chars(node_text(child, source), 4_000),
            start_line: start_line(child),
            end_line: end_line(child),
            body_stream: token_stream(child, source),
        });
    }
}

fn receiver_type(receiver: Node<'_>, source: &str) -> Option<String> {
    fn find(node: Node<'_>, source: &str) -> Option<String> {
        if node.kind() == "type_identifier" {
            return source.get(node.byte_range()).map(str::to_owned);
        }
        // `receiver` is a parameter_list; the type lives on the inner
        // parameter_declaration, so recurse through every named child.
        if matches!(
            node.kind(),
            "pointer_type" | "parameter_declaration" | "parameter_list" | "generic_type"
        ) {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                if let Some(found) = find(child, source) {
                    return Some(found);
                }
            }
        }
        None
    }
    let mut cursor = receiver.walk();
    let children: Vec<Node<'_>> = receiver.named_children(&mut cursor).collect();
    children.into_iter().find_map(|child| find(child, source))
}

fn collect_imports(root: Node<'_>, source: &str) -> String {
    let mut imports = Vec::new();
    walk(root, &mut |node| {
        if node.kind() == "import_declaration" {
            imports.push(collapse(node_text(node, source)));
        }
    });
    truncate_chars(&imports.join("\n"), 4_000)
}

fn collect_edges(node: Node<'_>, source: &str, qualified_name: &str, edges: &mut Vec<ParsedEdge>) {
    let mut calls = 0_usize;
    let mut types = 0_usize;
    let mut visit = |child: Node<'_>| match child.kind() {
        "call_expression" => {
            if calls >= 200 {
                return;
            }
            if let Some(function) = child.child_by_field_name("function") {
                if let Some(target) = callee(function, source) {
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: target,
                        relation: EdgeRelation::Calls,
                    });
                    calls += 1;
                }
            }
        }
        "composite_literal" | "type_assertion" | "type_conversion_expression" => {
            if types >= 100 {
                return;
            }
            if let Some(ty) = child.child_by_field_name("type") {
                if let Some(name) = type_name(ty, source) {
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: name,
                        relation: EdgeRelation::UsesType,
                    });
                    types += 1;
                }
            }
        }
        _ => {}
    };
    walk(node, &mut visit);
}

fn callee(function: Node<'_>, source: &str) -> Option<String> {
    match function.kind() {
        "identifier" => source.get(function.byte_range()).map(str::to_owned),
        "selector_expression" => function
            .child_by_field_name("field")
            .and_then(|field| source.get(field.byte_range()))
            .map(str::to_owned),
        _ => {
            let text = node_text(function, source);
            text.rsplit('.').next().map(str::to_owned)
        }
    }
}

fn type_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => source.get(node.byte_range()).map(str::to_owned),
        "pointer_type" | "generic_type" => {
            let mut cursor = node.walk();
            let found = node
                .named_children(&mut cursor)
                .find_map(|child| type_name(child, source));
            found
        }
        _ => None,
    }
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

#[allow(dead_code)]
fn is_comment(node: Node<'_>) -> bool {
    is_comment_kind(node.kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> ParsedFile {
        GoParser::new()
            .parse(&RepoPath::parse("internal/auth.go").unwrap(), source)
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
    fn functions_and_doc_comments() {
        let file = parse(
            "package auth\n\n// Authenticate checks a token.\nfunc Authenticate(token string) bool {\n\treturn validate(token)\n}\n",
        );
        let auth = symbol(&file, "Authenticate");
        assert_eq!(auth.kind, SymbolKind::Function);
        assert_eq!(
            auth.docstring.as_deref(),
            Some("Authenticate checks a token.")
        );
        assert_eq!(auth.start_line, 4);
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Authenticate"
                && edge.target_name == "validate"
                && edge.relation == EdgeRelation::Calls
        }));
    }

    #[test]
    fn methods_are_qualified_by_receiver() {
        let file = parse(
            "package store\n\ntype Store struct{}\n\nfunc (s *Store) Save() error { return nil }\n",
        );
        assert_eq!(symbol(&file, "Store").kind, SymbolKind::Struct);
        assert_eq!(symbol(&file, "Store::Save").kind, SymbolKind::Method);
    }

    #[test]
    fn interfaces_expose_method_signatures() {
        let file =
            parse("package io\n\ntype Reader interface {\n\tRead(p []byte) (int, error)\n}\n");
        assert_eq!(symbol(&file, "Reader").kind, SymbolKind::Interface);
        assert_eq!(symbol(&file, "Reader::Read").kind, SymbolKind::Method);
    }

    #[test]
    fn imports_are_collected() {
        let file = parse(
            "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() { fmt.Println(os.Args) }\n",
        );
        assert!(file.imports.contains("fmt"));
        assert!(file.imports.contains("os"));
    }

    #[test]
    fn composite_literals_are_type_uses() {
        let file = parse(
            "package main\n\ntype Config struct{}\n\nfunc build() Config { return Config{} }\n",
        );
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "build"
                && edge.target_name == "Config"
                && edge.relation == EdgeRelation::UsesType
        }));
    }
}
