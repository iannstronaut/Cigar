//! TypeScript, TSX, and JavaScript parser (docs/07 sections 4.2, 8.3, 9).
//!
//! One implementation serves TypeScript, TSX, and JavaScript because the node
//! kinds are identical apart from JSX and type-only nodes, which are simply
//! absent in the JavaScript grammar.

use std::sync::OnceLock;

use tree_sitter::{Language as TsLanguage, Node, Query, QueryCursor, StreamingIterator, Tree};

use crate::domain::{EdgeRelation, Language, RepoPath, SymbolKind};
use crate::traits::{LanguageParser, ParseError, ParsedEdge, ParsedFile, ParsedSymbol};

use super::common::{
    clause_type_names, collapse, end_line, node_name, node_text, parse_error, preceding_doc,
    signature_before_body, start_line, token_stream, walk, ParserCell, MAX_SYMBOLS_PER_FILE,
};

/// Which concrete grammar to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    TypeScript,
    Tsx,
    JavaScript,
}

impl Flavor {
    fn ts_language(self) -> TsLanguage {
        match self {
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }

    fn jsdoc_kinds(self) -> &'static [&'static str] {
        match self {
            Self::JavaScript => &["comment"],
            _ => &["comment"],
        }
    }
}

/// TypeScript/TSX parser.
///
/// One cell per grammar, created lazily on first use so constructing the parser
/// stays cheap.
pub struct TypeScriptParser {
    typescript: ParserCell,
    tsx: ParserCell,
    javascript: ParserCell,
}

impl TypeScriptParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            typescript: ParserCell::new(Flavor::TypeScript.ts_language()),
            tsx: ParserCell::new(Flavor::Tsx.ts_language()),
            javascript: ParserCell::new(Flavor::JavaScript.ts_language()),
        }
    }

    /// Parser preconfigured for `.tsx`.
    #[must_use]
    pub fn new_tsx() -> Self {
        Self::new()
    }

    fn cell_for(&self, flavor: Flavor) -> &ParserCell {
        match flavor {
            Flavor::TypeScript => &self.typescript,
            Flavor::Tsx => &self.tsx,
            Flavor::JavaScript => &self.javascript,
        }
    }
}

impl Default for TypeScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptParser {
    /// Parses with an explicit flavor; used by the JS parser and by tests.
    pub fn parse_flavor(
        &self,
        flavor: Flavor,
        path: &RepoPath,
        source: &str,
    ) -> Result<ParsedFile, ParseError> {
        let effective_flavor = if path.extension() == Some("tsx") {
            Flavor::Tsx
        } else {
            flavor
        };
        let tree: Tree = self
            .cell_for(effective_flavor)
            .parse(source)
            .ok_or_else(|| parse_error(path, "tree-sitter gagal memparse"))?;
        parse_tree(tree, effective_flavor, source, path)
    }
}

fn parse_tree(
    tree: Tree,
    flavor: Flavor,
    source: &str,
    path: &RepoPath,
) -> Result<ParsedFile, ParseError> {
    let root = tree.root_node();
    let mut out = ParsedFile::default();
    out.imports = collect_imports(root, source, flavor);

    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    collect_container(
        root,
        source,
        flavor,
        &mut Vec::new(),
        Container::Module,
        &mut symbols,
        &mut edges,
        path,
    )?;
    if symbols.len() > MAX_SYMBOLS_PER_FILE {
        symbols.truncate(MAX_SYMBOLS_PER_FILE);
    }
    out.symbols = symbols;
    out.edges = dedupe(edges);
    Ok(out)
}

/// What kind of container a node lives in, to qualify method names.
#[derive(Clone, Debug)]
enum Container {
    Module,
    Class(String),
    Interface(String),
    Namespace(String),
    Function(String),
}

impl Container {
    fn qualify(&self, name: &str, scope: &[String]) -> String {
        match self {
            Self::Module => {
                if scope.is_empty() {
                    name.to_owned()
                } else {
                    format!("{}::{name}", scope.join("::"))
                }
            }
            Self::Class(class) | Self::Interface(class) | Self::Namespace(class) => {
                format!("{class}::{name}")
            }
            Self::Function(function) => format!("{function}::{name}"),
        }
    }

    fn kind_for_function(&self) -> SymbolKind {
        match self {
            Self::Class(_) | Self::Interface(_) => SymbolKind::Method,
            _ => SymbolKind::Function,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_container(
    node: Node<'_>,
    source: &str,
    flavor: Flavor,
    scope: &mut Vec<String>,
    container: Container,
    symbols: &mut Vec<ParsedSymbol>,
    edges: &mut Vec<ParsedEdge>,
    path: &RepoPath,
) -> Result<(), ParseError> {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = container.qualify(name, scope);
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    container.kind_for_function(),
                    "body",
                    flavor,
                )?);
                let mut child_scope = scope.clone();
                child_scope.push(name.to_owned());
                collect_container(
                    child,
                    source,
                    flavor,
                    &mut child_scope,
                    Container::Function(name.to_owned()),
                    symbols,
                    edges,
                    path,
                )?;
                collect_edges(child, source, &qualified, None, edges);
            }
            "lexical_declaration" | "variable_declaration" => {
                for declarator in named_children_of_kind(child, "variable_declarator") {
                    let Some(name_node) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    if name_node.kind() != "identifier" {
                        continue;
                    }
                    let Some(name) = source.get(name_node.byte_range()) else {
                        continue;
                    };
                    let Some(value) = declarator.child_by_field_name("value") else {
                        continue;
                    };
                    if !matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "function"
                    ) {
                        continue;
                    }
                    let qualified = container.qualify(name, scope);
                    symbols.push(build_symbol(
                        value,
                        source,
                        name,
                        &qualified,
                        SymbolKind::Function,
                        "body",
                        flavor,
                    )?);
                    collect_edges(value, source, &qualified, None, edges);
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = container.qualify(name, scope);
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    SymbolKind::Class,
                    "",
                    flavor,
                )?);
                collect_edges(child, source, &qualified, Some(name), edges);
                collect_container(
                    child,
                    source,
                    flavor,
                    scope,
                    Container::Class(name.to_owned()),
                    symbols,
                    edges,
                    path,
                )?;
                // `class_heritage` wraps both `implements_clause` and
                // `extends_clause`; each listed type becomes an IMPLEMENTS edge
                // from the class (docs/07 section 8.3).
                let mut heritage_nodes: Vec<Node<'_>> = Vec::new();
                if let Some(heritage) = descendant_of_kind(child, "class_heritage") {
                    heritage_nodes.push(heritage);
                }
                for heritage in heritage_nodes {
                    for clause in ["implements_clause", "extends_clause"] {
                        if let Some(found) = descendant_of_kind(heritage, clause) {
                            for target in clause_type_names(found, source) {
                                edges.push(ParsedEdge {
                                    source_qualified_name: qualified.clone(),
                                    target_name: target,
                                    relation: EdgeRelation::Implements,
                                });
                            }
                        }
                    }
                }
            }
            "interface_declaration" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = container.qualify(name, scope);
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    SymbolKind::Interface,
                    "",
                    flavor,
                )?);
                if let Some(clause) = child
                    .child_by_field_name("extends")
                    .or_else(|| descendant_of_kind(child, "extends_clause"))
                {
                    for target in clause_type_names(clause, source) {
                        edges.push(ParsedEdge {
                            source_qualified_name: qualified.clone(),
                            target_name: target,
                            relation: EdgeRelation::Implements,
                        });
                    }
                }
                collect_container(
                    child,
                    source,
                    flavor,
                    scope,
                    Container::Interface(name.to_owned()),
                    symbols,
                    edges,
                    path,
                )?;
            }
            "enum_declaration" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = container.qualify(name, scope);
                symbols.push(build_symbol(
                    child,
                    source,
                    name,
                    &qualified,
                    SymbolKind::Enum,
                    "",
                    flavor,
                )?);
            }
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let (kind, qualified) = match &container {
                    Container::Class(class) => (SymbolKind::Method, format!("{class}::{name}")),
                    Container::Interface(interface) => {
                        (SymbolKind::Method, format!("{interface}::{name}"))
                    }
                    other => (SymbolKind::Function, other.qualify(name, scope)),
                };
                let body_field = if child.child_by_field_name("body").is_some() {
                    "body"
                } else {
                    ""
                };
                symbols.push(build_symbol(
                    child, source, name, &qualified, kind, body_field, flavor,
                )?);
                if !body_field.is_empty() {
                    let class_owner = match &container {
                        Container::Class(class) => Some(class.as_str()),
                        _ => None,
                    };
                    collect_edges(child, source, &qualified, class_owner, edges);
                }
            }
            "internal_module" | "module" => {
                let Some(name) = node_name(child, source) else {
                    continue;
                };
                let qualified = container.qualify(name, scope);
                collect_container(
                    child,
                    source,
                    flavor,
                    scope,
                    Container::Namespace(qualified),
                    symbols,
                    edges,
                    path,
                )?;
            }
            "export_statement" => {
                collect_container(
                    child,
                    source,
                    flavor,
                    scope,
                    container.clone(),
                    symbols,
                    edges,
                    path,
                )?;
            }
            _ => {
                collect_container(
                    child,
                    source,
                    flavor,
                    scope,
                    container.clone(),
                    symbols,
                    edges,
                    path,
                )?;
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
    flavor: Flavor,
) -> Result<ParsedSymbol, ParseError> {
    let signature = if body_field.is_empty() {
        collapse(node_text(node, source))
    } else {
        signature_before_body(node, source, body_field)
    };
    let code = truncate_chars(node_text(node, source), 12_000);
    let docstring =
        preceding_doc(node, source, flavor.jsdoc_kinds()).map(|doc| truncate_chars(&doc, 2_000));
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
        signature: truncate_chars(&signature, 500),
        docstring,
        code,
        start_line: start_line(node),
        end_line: end_line(node),
        body_stream: body,
    })
}

fn collect_imports(root: Node<'_>, source: &str, _flavor: Flavor) -> String {
    let mut imports = Vec::new();
    walk(root, &mut |node| match node.kind() {
        "import_statement" => {
            imports.push(collapse(node_text(node, source)));
        }
        "export_statement" => {
            let text = node_text(node, source);
            if text.contains(" from ") {
                imports.push(collapse(text));
            }
        }
        _ => {}
    });
    truncate_chars(&imports.join("\n"), 4_000)
}

fn collect_edges(
    node: Node<'_>,
    source: &str,
    qualified_name: &str,
    class_name: Option<&str>,
    edges: &mut Vec<ParsedEdge>,
) {
    let mut calls = 0_usize;
    let mut types = 0_usize;
    let mut visit = |child: Node<'_>| match child.kind() {
        "call_expression" => {
            if calls >= 200 {
                return;
            }
            if let Some(target) = call_target(child, source, class_name) {
                edges.push(ParsedEdge {
                    source_qualified_name: qualified_name.to_owned(),
                    target_name: target,
                    relation: EdgeRelation::Calls,
                });
                calls += 1;
            }
        }
        "new_expression" => {
            if types >= 100 {
                return;
            }
            if let Some(constructor) = child.child_by_field_name("constructor") {
                if let Some(name) = type_name(constructor, source) {
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: name.clone(),
                        relation: EdgeRelation::Calls,
                    });
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: name,
                        relation: EdgeRelation::UsesType,
                    });
                    types += 1;
                }
            }
        }
        "type_annotation" | "type_arguments" => {
            if types >= 100 {
                return;
            }
            if let Some(name) = type_name(child, source) {
                edges.push(ParsedEdge {
                    source_qualified_name: qualified_name.to_owned(),
                    target_name: name,
                    relation: EdgeRelation::UsesType,
                });
                types += 1;
            }
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if types >= 100 {
                return;
            }
            if let Some(name_node) = child.child_by_field_name("name") {
                let text = node_text(name_node, source);
                if text
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_uppercase())
                {
                    edges.push(ParsedEdge {
                        source_qualified_name: qualified_name.to_owned(),
                        target_name: text.to_owned(),
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

fn call_target(call: Node<'_>, source: &str, class_name: Option<&str>) -> Option<String> {
    let function = call.child_by_field_name("function")?;
    match function.kind() {
        "identifier" => source.get(function.byte_range()).map(str::to_owned),
        "member_expression" => {
            let property = function.child_by_field_name("property")?;
            let method = source.get(property.byte_range())?;
            let object = function.child_by_field_name("object")?;
            if node_text(object, source) == "this" {
                if let Some(class) = class_name {
                    return Some(format!("{class}::{method}"));
                }
            }
            Some(method.to_owned())
        }
        _ => {
            let text = node_text(function, source);
            text.split('<')
                .next()
                .map(|value| value.rsplit('.').next().unwrap_or(value).trim().to_owned())
                .filter(|value| !value.is_empty())
        }
    }
}

fn type_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => source.get(node.byte_range()).map(str::to_owned),
        "generic_type" => node
            .child_by_field_name("name")
            .and_then(|inner| type_name(inner, source)),
        "nested_type_identifier" | "qualified_name" => {
            let text = node_text(node, source);
            Some(text.rsplit('.').next().unwrap_or(text).to_owned())
        }
        "type_annotation" => {
            let mut cursor = node.walk();
            let first = node.named_children(&mut cursor).next()?;
            type_name(first, source)
        }
        "type_arguments" => {
            let mut cursor = node.walk();
            let first = node.named_children(&mut cursor).next()?;
            type_name(first, source)
        }
        "predefined_type" => None,
        _ => {
            let text = node_text(node, source);
            let candidate = text
                .trim_start_matches(':')
                .trim()
                .split(['<', '[', '|', '&', ' '])
                .next()?
                .rsplit('.')
                .next()?
                .trim();
            if candidate.is_empty()
                || !candidate
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '$')
            {
                None
            } else {
                Some(candidate.to_owned())
            }
        }
    }
}

fn named_children_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == kind)
        .collect()
}

fn descendant_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let direct: Vec<Node<'tree>> = {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).collect()
    };
    if let Some(found) = direct.iter().find(|child| child.kind() == kind) {
        return Some(*found);
    }
    direct
        .into_iter()
        .find_map(|child| descendant_of_kind(child, kind))
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

/// Query used by tests to verify node kinds.
#[allow(dead_code)]
fn declaration_query() -> &'static Query {
    static QUERY: OnceLock<Query> = OnceLock::new();
    QUERY.get_or_init(|| {
        let language: TsLanguage = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        Query::new(
            &language,
            "(function_declaration name: (identifier) @fn)
             (class_declaration name: (type_identifier) @class)
             (interface_declaration name: (type_identifier) @interface)
             (enum_declaration name: (identifier) @enum)",
        )
        .expect("query harus valid")
    })
}

#[allow(dead_code)]
fn query_names(root: Node<'_>, source: &str) -> Vec<String> {
    let query = declaration_query();
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

impl LanguageParser for TypeScriptParser {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "mts", "cts", "tsx"]
    }

    fn parse(&self, path: &RepoPath, source: &str) -> Result<ParsedFile, ParseError> {
        let flavor = if path.extension() == Some("tsx") {
            Flavor::Tsx
        } else {
            Flavor::TypeScript
        };
        self.parse_flavor(flavor, path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> ParsedFile {
        let parser = TypeScriptParser::new();
        parser
            .parse(&RepoPath::parse("src/app.ts").unwrap(), source)
            .unwrap()
    }

    fn parse_tsx(source: &str) -> ParsedFile {
        let parser = TypeScriptParser::new_tsx();
        parser
            .parse(&RepoPath::parse("src/app.tsx").unwrap(), source)
            .unwrap()
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
    fn extracts_function_and_arrow() {
        let file = parse(
            "/** Adds. */\nexport function add(a: number, b: number): number {\n  return a + b;\n}\n\
             export const mul = (a: number) => a * 2;\n",
        );
        let add = symbol(&file, "add");
        assert_eq!(add.kind, SymbolKind::Function);
        assert_eq!(add.docstring.as_deref(), Some("Adds."));
        // The `export` keyword is outside the `function_declaration` node, so
        // the signature starts at the `function` keyword (docs/07 section 5.1).
        assert_eq!(add.signature, "function add(a: number, b: number): number");
        let mul = symbol(&file, "mul");
        assert_eq!(mul.kind, SymbolKind::Function);
        assert!(mul.code.contains("=>"));
    }

    #[test]
    fn extracts_class_methods_and_ctor() {
        let file = parse(
            "class Circle {\n  radius: number;\n  constructor(r: number) { this.radius = r; }\n  area(): number { return 3.14 * this.radius; }\n}\n",
        );
        assert_eq!(symbol(&file, "Circle").kind, SymbolKind::Class);
        assert_eq!(
            symbol(&file, "Circle::constructor").kind,
            SymbolKind::Method
        );
        assert_eq!(symbol(&file, "Circle::area").kind, SymbolKind::Method);
    }

    #[test]
    fn interface_and_enum_kinds() {
        let file = parse("interface Shape { area(): number; }\nenum Color { Red, Green }\n");
        assert_eq!(symbol(&file, "Shape").kind, SymbolKind::Interface);
        assert_eq!(symbol(&file, "Shape::area").kind, SymbolKind::Method);
        assert_eq!(symbol(&file, "Color").kind, SymbolKind::Enum);
    }

    #[test]
    fn implements_and_extends_become_edges() {
        let file = parse("interface Drawable {}\nclass Square implements Drawable { draw() {} }\n");
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Square"
                && edge.target_name == "Drawable"
                && edge.relation == EdgeRelation::Implements
        }));
    }

    #[test]
    fn this_call_uses_class_qualification() {
        let file = parse(
            "class Auth {\n  validate() { return true; }\n  run() { return this.validate(); }\n}\n",
        );
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Auth::run"
                && edge.target_name == "Auth::validate"
                && edge.relation == EdgeRelation::Calls
        }));
    }

    #[test]
    fn new_expression_yields_calls_and_uses_type() {
        let file = parse("function build() { return new Service(); }\n");
        let edges: Vec<&ParsedEdge> = file
            .edges
            .iter()
            .filter(|edge| edge.source_qualified_name == "build")
            .collect();
        assert!(edges
            .iter()
            .any(|edge| edge.target_name == "Service" && edge.relation == EdgeRelation::Calls));
        assert!(edges
            .iter()
            .any(|edge| edge.target_name == "Service" && edge.relation == EdgeRelation::UsesType));
    }

    #[test]
    fn namespace_qualifies_inner_functions() {
        let file = parse("namespace Auth { export function login() {} }\n");
        assert!(file
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Auth::login" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn imports_are_collected() {
        let file = parse(
            "import { foo } from \"./foo\";\nexport { bar } from \"./bar\";\nimport type { T } from \"t\";\n",
        );
        assert!(file.imports.contains("from \"./foo\""));
        assert!(file.imports.contains("from \"./bar\""));
        assert!(file.imports.contains("from \"t\""));
        assert_eq!(file.imports.lines().count(), 3);
    }

    #[test]
    fn tsx_jsx_components_are_type_uses() {
        let file = parse_tsx("function Card() { return <Button label=\"x\" />; }\n");
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "Card"
                && edge.target_name == "Button"
                && edge.relation == EdgeRelation::UsesType
        }));
    }

    #[test]
    fn type_annotations_become_type_edges() {
        let file = parse("function take(x: Service) { return x; }\n");
        assert!(file.edges.iter().any(|edge| {
            edge.source_qualified_name == "take"
                && edge.target_name == "Service"
                && edge.relation == EdgeRelation::UsesType
        }));
    }

    #[test]
    fn query_helper_finds_declarations() {
        let source = "function a() {}\nclass B {}\ninterface C {}\nenum D { X }\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let names = query_names(tree.root_node(), source);
        assert_eq!(names, vec!["a", "B", "C", "D"]);
    }
}
