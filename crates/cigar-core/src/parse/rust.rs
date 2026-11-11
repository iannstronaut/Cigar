// SPDX-License-Identifier: MPL-2.0

//! Rust parser (docs/07 sections 4.1, 8.2, 9).

use tree_sitter::{Node, Tree};

use crate::domain::{EdgeRelation, Language, RepoPath, SymbolKind};
use crate::traits::{LanguageParser, ParseError, ParsedEdge, ParsedFile, ParsedSymbol};

use super::common::{
    collapse, end_line, node_name, node_text, parse_error, preceding_doc, signature_before_body,
    start_line, token_stream, walk, ParserCell, MAX_SYMBOLS_PER_FILE,
};

/// Rust parser.
pub struct RustParser {
    cell: ParserCell,
}

impl RustParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cell: ParserCell::new(tree_sitter_rust::LANGUAGE.into()),
        }
    }

    fn tree<'a>(&self, path: &RepoPath, source: &'a str) -> Result<Tree, ParseError> {
        self.cell
            .parse(source)
            .ok_or_else(|| parse_error(path, "tree-sitter gagal memparse"))
    }
}

impl Default for RustParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for RustParser {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn extensions(&self) -> &'static [&'static str] {
        super::RUST_EXTENSIONS
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
            ImplContext::None,
            &mut symbols,
            &mut edges,
            path,
        )?;

        if symbols.len() > MAX_SYMBOLS_PER_FILE {
            symbols.truncate(MAX_SYMBOLS_PER_FILE);
        }
        out.symbols = symbols;
        out.edges = dedupe_edges(edges);
        Ok(out)
    }
}

/// How a `function_item` is qualified based on its enclosing `impl`.
#[derive(Clone, Debug)]
enum ImplContext {
    None,
    /// `impl Type` -> `Type::method`
    Inherent(String),
    /// `impl Trait for Type` -> `<Type as Trait>::method`
    TraitFor {
        trait_name: String,
        type_name: String,
    },
    /// Inside a trait body -> `Trait::method`
    Trait(String),
}

impl ImplContext {
    fn qualify(&self, name: &str, source_name: &str) -> String {
        match self {
            Self::None => source_name.to_owned(),
            Self::Inherent(ty) => format!("{ty}::{name}"),
            Self::TraitFor {
                trait_name,
                type_name,
            } => format!("<{type_name} as {trait_name}>::{name}"),
            Self::Trait(trait_name) => format!("{trait_name}::{name}"),
        }
    }

    fn kind(&self) -> SymbolKind {
        match self {
            Self::None => SymbolKind::Function,
            _ => SymbolKind::Method,
        }
    }
}

impl ImplContext {
    fn self_type_str(&self) -> Option<&str> {
        match self {
            Self::Inherent(ty) => Some(ty.as_str()),
            Self::TraitFor { type_name, .. } => Some(type_name.as_str()),
            _ => None,
        }
    }
}

fn collect_items<'t>(
    node: Node<'t>,
    source: &'t str,
    scope: &mut Vec<String>,
    context: ImplContext,
    symbols: &mut Vec<ParsedSymbol>,
    edges: &mut Vec<ParsedEdge>,
    path: &RepoPath,
) -> Result<(), ParseError> {
    let mut cursor = node.walk();
    let children: Vec<Node<'t>> = node.named_children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "function_item" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let source_qname = if scope.is_empty() {
                    name.to_owned()
                } else {
                    format!("{}::{name}", scope.join("::"))
                };
                let qualified = context.qualify(name, &source_qname);
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    context.kind(),
                    "body",
                )?);
                let mut child_scope = scope.clone();
                child_scope.push(name.to_owned());
                collect_items(
                    child,
                    source,
                    &mut child_scope,
                    ImplContext::None,
                    symbols,
                    edges,
                    path,
                )?;
                collect_function_edges(child, source, &qualified, context.self_type_str(), edges);
            }
            "function_signature_item" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = match &context {
                    ImplContext::Trait(trait_name) => format!("{trait_name}::{name}"),
                    other => other.qualify(name, name),
                };
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    SymbolKind::Method,
                    "",
                )?);
            }
            "struct_item" | "enum_item" | "trait_item" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let kind = match child.kind() {
                    "struct_item" => SymbolKind::Struct,
                    "enum_item" => SymbolKind::Enum,
                    _ => SymbolKind::Trait,
                };
                let qualified = if scope.is_empty() {
                    name.to_owned()
                } else {
                    format!("{}::{name}", scope.join("::"))
                };
                symbols.push(build_symbol(child, source, name, &qualified, kind, "")?);

                if child.kind() == "trait_item" {
                    collect_items(
                        child,
                        source,
                        scope,
                        ImplContext::Trait(name.to_owned()),
                        symbols,
                        edges,
                        path,
                    )?;
                } else {
                    let mut child_scope = scope.clone();
                    child_scope.push(name.to_owned());
                    collect_items(
                        child,
                        source,
                        &mut child_scope,
                        ImplContext::None,
                        symbols,
                        edges,
                        path,
                    )?;
                }
            }
            "impl_item" => {
                let type_name = child
                    .child_by_field_name("type")
                    .and_then(|node| type_name_of(node, source));
                let trait_name = child
                    .child_by_field_name("trait")
                    .and_then(|node| type_name_of(node, source));
                let inner = match (&trait_name, &type_name) {
                    (Some(trait_name), Some(type_name)) => ImplContext::TraitFor {
                        trait_name: trait_name.clone(),
                        type_name: type_name.clone(),
                    },
                    (None, Some(type_name)) => ImplContext::Inherent(type_name.clone()),
                    (Some(trait_name), None) => ImplContext::Trait(trait_name.clone()),
                    (None, None) => ImplContext::None,
                };
                if let (Some(trait_name), Some(type_name)) = (&trait_name, &type_name) {
                    edges.push(ParsedEdge {
                        source_qualified_name: type_name.clone(),
                        target_name: trait_name.clone(),
                        relation: EdgeRelation::Implements,
                    });
                }
                collect_items(child, source, scope, inner, symbols, edges, path)?;
            }
            "mod_item" => {
                // Inline modules become scope segments; declaration-only mods
                // were already captured as imports.
                let has_body = child.child_by_field_name("body").is_some();
                if has_body {
                    let name = node_name(child, source).unwrap_or("<mod>");
                    let mut child_scope = scope.clone();
                    child_scope.push(name.to_owned());
                    collect_items(
                        child,
                        source,
                        &mut child_scope,
                        ImplContext::None,
                        symbols,
                        edges,
                        path,
                    )?;
                }
            }
            _ => {
                collect_items(child, source, scope, context.clone(), symbols, edges, path)?;
            }
        }
    }
    Ok(())
}

fn build_symbol(
    node: Node<'_>,
    source: &str,
    name: &str,
    qualified_name: &str,
    kind: SymbolKind,
    body_field: &str,
) -> Result<ParsedSymbol, ParseError> {
    let signature = if body_field.is_empty() {
        collapse(node_text(node, source))
    } else {
        signature_before_body(node, source, body_field)
    };
    let signature = truncate_chars(&signature, 500);
    let code = truncate_chars(node_text(node, source), 12_000);
    let docstring = preceding_doc(node, source, &["line_comment", "block_comment"]);
    let docstring = docstring.map(|doc| truncate_chars(&doc, 2_000));
    let body = if body_field.is_empty() {
        token_stream(node, source)
    } else {
        node.child_by_field_name(body_field).map_or_else(
            || token_stream(node, source),
            |body| token_stream(body, source),
        )
    };

    Ok(ParsedSymbol {
        name: name.to_owned(),
        qualified_name: qualified_name.to_owned(),
        kind,
        signature,
        docstring,
        code,
        start_line: start_line(node),
        end_line: end_line(node),
        body_stream: body,
    })
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(limit.saturating_sub(1)).collect();
    out.push('~');
    out
}

/// Extracts a type name from a `type_identifier`, `generic_type`, or path.
fn type_name_of(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => source.get(node.byte_range()).map(str::to_owned),
        "generic_type" => node
            .child_by_field_name("type")
            .and_then(|inner| type_name_of(inner, source)),
        "scoped_type_identifier" => {
            let text = node_text(node, source);
            let tail = text.rsplit("::").next().unwrap_or(text);
            Some(tail.to_owned())
        }
        "reference_type" | "pointer_type" => node
            .child_by_field_name("type")
            .and_then(|inner| type_name_of(inner, source)),
        _ => None,
    }
}

fn collect_imports(root: Node<'_>, source: &str) -> String {
    let mut imports = Vec::new();
    walk(root, &mut |node| match node.kind() {
        // Keep the source form verbatim (including the trailing `;`) so the
        // embedded context matches what a developer reads (docs/07 section 6).
        "use_declaration" | "extern_crate_declaration" => {
            imports.push(collapse(node_text(node, source)));
        }
        // Only declaration-only modules (`mod foo;`) are imports.
        "mod_item" if node.child_by_field_name("body").is_none() => {
            imports.push(collapse(node_text(node, source)));
        }
        _ => {}
    });
    let joined = imports.join("\n");
    truncate_chars(&joined, 4_000)
}

fn collect_function_edges(
    node: Node<'_>,
    source: &str,
    qualified_name: &str,
    self_type: Option<&str>,
    edges: &mut Vec<ParsedEdge>,
) {
    let mut calls = 0_usize;
    let mut types = 0_usize;
    let mut visit = |child: Node<'_>| {
        if calls >= 200 && types >= 100 {
            return;
        }
        match child.kind() {
            "call_expression" => {
                if calls >= 200 {
                    return;
                }
                if let Some(target) = call_target(child, source, self_type) {
                    if !is_builtin_macro(&target) {
                        edges.push(ParsedEdge {
                            source_qualified_name: qualified_name.to_owned(),
                            target_name: target,
                            relation: EdgeRelation::Calls,
                        });
                        calls += 1;
                    }
                }
            }
            "struct_expression" => {
                if types >= 100 {
                    return;
                }
                if let Some(name) = child
                    .child_by_field_name("name")
                    .and_then(|node| type_name_of(node, source))
                {
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: name,
                        relation: EdgeRelation::UsesType,
                    });
                    types += 1;
                }
            }
            _ => {}
        }
    };
    walk(node, &mut visit);

    // Parameter and return types.
    if let Some(params) = node.child_by_field_name("parameters") {
        collect_param_types(params, source, qualified_name, edges);
    }
    if let Some(return_type) = node.child_by_field_name("return_type") {
        if let Some(name) = type_name_of(return_type, source) {
            edges.push(ParsedEdge {
                source_qualified_name: qualified_name.to_owned(),
                target_name: name,
                relation: EdgeRelation::UsesType,
            });
        }
    }
}

fn collect_param_types(
    params: Node<'_>,
    source: &str,
    qualified_name: &str,
    edges: &mut Vec<ParsedEdge>,
) {
    let mut cursor = params.walk();
    for param in params.named_children(&mut cursor) {
        let ty = param
            .child_by_field_name("type")
            .or_else(|| (param.kind() == "type_identifier").then_some(param));
        if let Some(ty) = ty {
            if let Some(name) = type_name_of(ty, source) {
                if !is_primitive(&name) {
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: name,
                        relation: EdgeRelation::UsesType,
                    });
                }
            }
        }
    }
}

fn call_target(call: Node<'_>, source: &str, self_type: Option<&str>) -> Option<String> {
    let function = call.child_by_field_name("function")?;
    match function.kind() {
        "identifier" => source.get(function.byte_range()).map(str::to_owned),
        "scoped_identifier" | "generic_function" => {
            let text = node_text(function, source);
            let text = text.split('<').next().unwrap_or(text);
            Some(text.trim().to_owned())
        }
        "field_expression" => {
            let field = function.child_by_field_name("field")?;
            let method = source.get(field.byte_range())?;
            let value = function.child_by_field_name("value")?;
            let receiver = node_text(value, source);
            if receiver == "self" {
                if let Some(ty) = self_type {
                    return Some(format!("{ty}::{method}"));
                }
            }
            Some(method.to_owned())
        }
        "macro_invocation" => {
            let macro_node = function.child_by_field_name("macro")?;
            Some(node_text(macro_node, source).to_owned())
        }
        _ => {
            let text = node_text(function, source);
            let tail = text.rsplit("::").next().unwrap_or(text);
            if tail.is_empty() {
                None
            } else {
                Some(tail.to_owned())
            }
        }
    }
}

fn is_builtin_macro(name: &str) -> bool {
    matches!(
        name,
        "println"
            | "print"
            | "eprintln"
            | "eprint"
            | "format"
            | "vec"
            | "write"
            | "writeln"
            | "panic"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "todo"
            | "unimplemented"
            | "unreachable"
            | "matches"
            | "include"
            | "include_str"
            | "include_bytes"
            | "cfg"
            | "stringify"
            | "concat"
            | "env"
            | "option_env"
            | "dbg"
    )
}

fn is_primitive(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
            | "String"
            | "Self"
            | "self"
    )
}

fn dedupe_edges(edges: Vec<ParsedEdge>) -> Vec<ParsedEdge> {
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

/// Query used by tests to assert extraction completeness.
#[cfg(test)]
fn symbol_query() -> &'static tree_sitter::Query {
    static QUERY: std::sync::OnceLock<tree_sitter::Query> = std::sync::OnceLock::new();
    QUERY.get_or_init(|| {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        tree_sitter::Query::new(
            &language,
            "(function_item name: (identifier) @fn_item)
             (struct_item name: (type_identifier) @struct_item)
             (enum_item name: (type_identifier) @enum_item)
             (trait_item name: (type_identifier) @trait_item)",
        )
        .expect("query harus valid")
    })
}

/// Runs the query, returning captured names (diagnostics/tests only).
#[cfg(test)]
fn query_names(root: Node<'_>, source: &str) -> Vec<String> {
    use tree_sitter::{QueryCursor, StreamingIterator};
    let query = symbol_query();
    let mut cursor = QueryCursor::new();
    let mut names = Vec::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(match_) = matches.next() {
        for capture in match_.captures() {
            if let Ok(text) = capture.node.utf8_text(source.as_bytes()) {
                names.push(text.to_owned());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> ParsedFile {
        let parser = RustParser::new();
        let path = RepoPath::parse("src/lib.rs").unwrap();
        parser.parse(&path, source).unwrap()
    }

    fn symbol<'a>(file: &'a ParsedFile, qualified: &str) -> &'a ParsedSymbol {
        file.symbols
            .iter()
            .find(|symbol| symbol.qualified_name == qualified)
            .unwrap_or_else(|| {
                panic!(
                    "simbol {qualified} tidak ditemukan; ada {:?}",
                    file.symbols
                        .iter()
                        .map(|s| s.qualified_name.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn extracts_top_level_function() {
        let file = parse("/// Menghitung.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        let add = symbol(&file, "add");
        assert_eq!(add.name, "add");
        assert_eq!(add.kind, SymbolKind::Function);
        assert_eq!(add.signature, "pub fn add(a: i32, b: i32) -> i32");
        assert_eq!(add.docstring.as_deref(), Some("Menghitung."));
        assert_eq!(add.start_line, 2);
        assert_eq!(add.end_line, 4);
    }

    #[test]
    fn inherent_impl_methods_are_qualified() {
        let file = parse(
            "struct Point { x: i32 }\nimpl Point {\n    pub fn new() -> Self { Point { x: 0 } }\n}\n",
        );
        assert_eq!(symbol(&file, "Point").kind, SymbolKind::Struct);
        assert_eq!(symbol(&file, "Point::new").kind, SymbolKind::Method);
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Point::new"
                && edge.target_name == "Point"
                && edge.relation == EdgeRelation::UsesType
        }));
    }

    #[test]
    fn trait_impl_methods_use_disambiguated_name() {
        let file = parse(
            "trait Draw { fn draw(&self); }\n\
             struct Circle;\n\
             impl Draw for Circle { fn draw(&self) {} }\n",
        );
        assert!(file
            .symbols
            .iter()
            .any(|s| s.qualified_name == "<Circle as Draw>::draw"));
        assert!(file
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Draw::draw"));
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Circle"
                && edge.target_name == "Draw"
                && edge.relation == EdgeRelation::Implements
        }));
    }

    #[test]
    fn nested_functions_are_qualified_by_scope() {
        let file = parse("fn outer() {\n    fn inner() {}\n}\n");
        // docs/07 section 4.1: a fn nested in another fn is `outer::inner`.
        assert!(file
            .symbols
            .iter()
            .any(|s| s.qualified_name == "outer::inner"));
        assert!(file.symbols.iter().any(|s| s.qualified_name == "outer"));
    }

    #[test]
    fn calls_and_self_calls_are_extracted() {
        let file = parse(
            "struct S;\nimpl S {\n  fn helper(&self) {}\n  fn run(&self) { self.helper(); other(); }\n}\n",
        );
        let run_edges: Vec<&ParsedEdge> = file
            .edges
            .iter()
            .filter(|edge| edge.source_qualified_name == "S::run")
            .collect();
        assert!(run_edges
            .iter()
            .any(|edge| edge.target_name == "S::helper" && edge.relation == EdgeRelation::Calls));
        assert!(run_edges
            .iter()
            .any(|edge| edge.target_name == "other" && edge.relation == EdgeRelation::Calls));
    }

    #[test]
    fn imports_include_use_and_mod_declarations() {
        let file = parse("use axum::http::Request;\nmod parser;\nmod inline { pub fn x() {} }\n");
        assert!(file.imports.contains("use axum::http::Request"));
        assert!(file.imports.contains("mod parser"));
        assert!(!file.imports.contains("mod inline"));
    }

    #[test]
    fn body_stream_ignores_comments_and_whitespace() {
        let compact = parse("fn f(a: i32) -> i32 { a + 1 }\n");
        let expanded = parse("fn f(a: i32) -> i32 {\n    a+1 // komentar\n}\n");
        assert_eq!(
            symbol(&compact, "f").body_stream,
            symbol(&expanded, "f").body_stream
        );
        let changed = parse("fn f(a: i32) -> i32 { a - 1 }\n");
        assert_ne!(
            symbol(&compact, "f").body_stream,
            symbol(&changed, "f").body_stream
        );
    }

    #[test]
    fn enum_and_trait_kinds_are_preserved() {
        let file = parse("enum Color { Red }\ntrait Shape { fn area(&self) -> f64; }\n");
        assert_eq!(symbol(&file, "Color").kind, SymbolKind::Enum);
        assert_eq!(symbol(&file, "Shape").kind, SymbolKind::Trait);
        assert_eq!(symbol(&file, "Shape::area").kind, SymbolKind::Method);
    }

    #[test]
    fn symbols_are_deterministic() {
        let source = "fn a() {}\nfn b() {}\nstruct C;\n";
        let first = parse(source);
        let second = parse(source);
        assert_eq!(first, second);
    }

    #[test]
    fn query_helper_finds_declarations() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let source = "fn a() {}\nstruct B;\nenum C { X }\ntrait D {}\n";
        let tree = parser.parse(source, None).unwrap();
        let names = query_names(tree.root_node(), source);
        assert_eq!(names, vec!["a", "B", "C", "D"]);
    }
}
