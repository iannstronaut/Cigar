//! JavaScript parser. Shares the TypeScript extractor with the JS grammar, which
//! keeps symbol/edge semantics identical across the JS family (docs/07).

use crate::domain::{Language, RepoPath};
use crate::traits::{LanguageParser, ParseError, ParsedFile};

use super::typescript::{Flavor, TypeScriptParser};

/// JavaScript parser.
pub struct JavaScriptParser {
    inner: TypeScriptParser,
}

impl JavaScriptParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: TypeScriptParser::new(),
        }
    }
}

impl Default for JavaScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for JavaScriptParser {
    fn language(&self) -> Language {
        Language::Javascript
    }

    fn extensions(&self) -> &'static [&'static str] {
        super::JAVASCRIPT_EXTENSIONS
    }

    fn parse(&self, path: &RepoPath, source: &str) -> Result<ParsedFile, ParseError> {
        self.inner.parse_flavor(Flavor::JavaScript, path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SymbolKind;

    fn parse(source: &str) -> ParsedFile {
        let parser = JavaScriptParser::new();
        parser
            .parse(&RepoPath::parse("src/app.js").unwrap(), source)
            .unwrap()
    }

    #[test]
    fn function_declarations_are_found() {
        let file = parse("function greet(name) {\n  return `hi ${name}`;\n}\n");
        assert_eq!(file.symbols.len(), 1);
        assert_eq!(file.symbols[0].qualified_name, "greet");
        assert_eq!(file.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn arrow_functions_are_named_from_bindings() {
        let file = parse("const load = async (id) => {\n  return fetch(id);\n};\n");
        let load = &file.symbols[0];
        assert_eq!(load.qualified_name, "load");
        assert!(load.code.contains("=>"));
    }

    #[test]
    fn classes_and_methods_are_found() {
        let file = parse("class Store {\n  get size() { return 0; }\n  save() {}\n}\n");
        assert!(file
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Store" && s.kind == SymbolKind::Class));
        assert!(file
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Store::save" && s.kind == SymbolKind::Method));
    }
}
