// SPDX-License-Identifier: MPL-2.0

//! Hashing and embedding-template rendering.
//!
//! All identifiers are domain-separated BLAKE3 digests with `0x00` field
//! separators (docs/03 ?4.1, docs/21 ?2). The template renderer is the single
//! source of truth for the text that gets embedded, so `embedding_key` always
//! matches the exact bytes sent to the model.

use crate::domain::SymbolDef;
use crate::domain::{Digest, EmbeddingKey, Language, RepoPath, SymbolKind};
use crate::error::Result;

/// A rendered embedding template plus bookkeeping about truncation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingTemplate {
    text: String,
    truncated_code: bool,
    truncated_header: bool,
}

impl EmbeddingTemplate {
    /// The exact text that must be embedded and keyed.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the template, returning the exact text.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    /// True when the code section had to be shortened to fit the token budget.
    #[must_use]
    pub fn truncated_code(&self) -> bool {
        self.truncated_code
    }

    /// True when imports/doc/signature had to be shortened.
    #[must_use]
    pub fn truncated_header(&self) -> bool {
        self.truncated_header
    }

    /// Renders the template without a token budget, only the character cap on
    /// code (docs/08 ?5.1).
    #[must_use]
    pub fn render(path: &RepoPath, imports: &str, symbol: &SymbolDef) -> Self {
        Self::render_with_budget(path, imports, symbol, None, None)
    }

    /// Renders and, when a token counter is supplied, truncates to `max_tokens`
    /// in the order required by docs/08 ?5.2: header first, then code lines.
    #[must_use]
    pub fn render_with_budget(
        path: &RepoPath,
        imports: &str,
        symbol: &SymbolDef,
        max_tokens: Option<usize>,
        counter: Option<&dyn Fn(&str) -> usize>,
    ) -> Self {
        let mut imports = normalize_inline(imports);
        let mut doc = symbol
            .docstring
            .as_deref()
            .map(normalize_doc)
            .filter(|d| !d.is_empty());
        let mut signature = normalize_inline(&symbol.signature);
        if signature.is_empty() {
            signature = symbol.name.clone();
        }
        let mut code = normalize_code(&symbol.code_snippet);

        let build = |imports: &str, doc: Option<&str>, signature: &str, code: &str| {
            let mut out = String::with_capacity(code.len() + 256);
            out.push_str("path: ");
            out.push_str(path.as_str());
            out.push('\n');
            if !imports.is_empty() {
                out.push_str("imports: ");
                out.push_str(imports);
                out.push('\n');
            }
            out.push_str("symbol: ");
            out.push_str(signature);
            out.push('\n');
            if let Some(doc) = doc {
                out.push_str("doc: ");
                out.push_str(doc);
                out.push('\n');
            }
            out.push_str("---\n");
            out.push_str(code);
            out
        };

        let mut truncated_header = false;
        let mut truncated_code = false;
        let mut text = build(&imports, doc.as_deref(), &signature, &code);
        if let (Some(limit), Some(count)) = (max_tokens, counter) {
            if count(&text) > limit {
                // 1. Shrink imports, keeping whole statements.
                truncated_header |=
                    shrink_imports(&mut imports, limit, count, &signature, &code, path);
                text = build(&imports, doc.as_deref(), &signature, &code);
            }
            if count(&text) > limit {
                // 2. Drop doc entirely, then shorten it progressively.
                if doc.take().is_some() {
                    truncated_header = true;
                    text = build(&imports, None, &signature, &code);
                }
            }
            if count(&text) > limit {
                // 3. Shorten signature to a hard character budget.
                let shortened = truncate_chars(&signature, signature.len().min(200), true);
                if shortened != signature {
                    truncated_header = true;
                    signature = shortened;
                    text = build(&imports, doc.as_deref(), &signature, &code);
                }
            }
            if count(&text) > limit {
                // 4. Truncate code by whole lines until it fits.
                let fixed = build(&imports, doc.as_deref(), &signature, "");
                let fixed_tokens = count(&fixed);
                let remaining = limit.saturating_sub(fixed_tokens);
                if remaining == 0 {
                    code.clear();
                } else {
                    code = truncate_to_tokens(&code, remaining, count);
                }
                truncated_code = true;
                text = build(&imports, doc.as_deref(), &signature, &code);
            }
            // 5. Last resort: hard character truncation so the contract holds.
            if count(&text) > limit {
                text = truncate_chars(&text, text.len().saturating_sub(1), true);
                truncated_code = true;
            }
        }

        Self {
            text,
            truncated_code,
            truncated_header,
        }
    }

    /// `embedding_key` for this exact text (docs/07 ?7.2).
    #[must_use]
    pub fn key(&self, template_version: u32) -> EmbeddingKey {
        embedding_key(template_version, &self.text)
    }
}

fn shrink_imports(
    imports: &mut String,
    limit: usize,
    count: &dyn Fn(&str) -> usize,
    signature: &str,
    code: &str,
    path: &RepoPath,
) -> bool {
    let build = |imports: &str| {
        let mut out = String::new();
        out.push_str("path: ");
        out.push_str(path.as_str());
        out.push('\n');
        if !imports.is_empty() {
            out.push_str("imports: ");
            out.push_str(imports);
            out.push('\n');
        }
        out.push_str("symbol: ");
        out.push_str(signature);
        out.push('\n');
        out.push_str("---\n");
        out.push_str(code);
        out
    };
    let lines: Vec<&str> = imports.lines().collect();
    for keep in (0..lines.len()).rev() {
        let candidate = lines[..keep].join(" ");
        if count(&build(&candidate)) <= limit {
            let changed = candidate != *imports;
            *imports = candidate;
            return changed;
        }
    }
    let changed = !imports.is_empty();
    imports.clear();
    changed
}

fn truncate_to_tokens(text: &str, token_budget: usize, count: &dyn Fn(&str) -> usize) -> String {
    if count(text) <= token_budget {
        return text.to_owned();
    }
    let mut lines: Vec<&str> = text.lines().collect();
    while !lines.is_empty() {
        lines.pop();
        let candidate = format!("{}\n?", lines.join("\n"));
        if count(&candidate) <= token_budget {
            return candidate;
        }
    }
    "?".to_owned()
}

/// Normalizes a multi-line code block: LF, trailing whitespace stripped,
/// leading/trailing blank lines removed (docs/07 ?5.3).
#[must_use]
pub fn normalize_code(input: &str) -> String {
    let unified = input.replace("\r\n", "\n");
    let mut out: Vec<&str> = unified.lines().map(str::trim_end).collect();
    while out.first().is_some_and(|line| line.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    collapse_blank_lines(out)
}

fn collapse_blank_lines(lines: Vec<&str>) -> String {
    let mut out = String::new();
    let mut blank_run = 0_usize;
    for line in lines {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Collapses all whitespace runs (including newlines) to one space and trims.
#[must_use]
pub fn normalize_inline(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalizes a docstring: strips common comment markers, trims each line and
/// collapses runs of blank lines to one (docs/07 ?5.2).
#[must_use]
pub fn normalize_doc(input: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for raw in input.replace("\r\n", "\n").lines() {
        let mut line = raw.trim().to_owned();
        if let Some(rest) = line.strip_prefix("///") {
            line = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("//!") {
            line = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("//") {
            line = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("/**") {
            line = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix('*') {
            line = rest.trim().to_owned();
        }
        if line.ends_with("*/") {
            line = line.trim_end_matches("*/").trim().to_owned();
        }
        lines.push(line);
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    collapse_blank_lines(lines.iter().map(String::as_str).collect())
}

fn truncate_chars(input: &str, max_chars: usize, ellipsis: bool) -> String {
    if input.chars().count() <= max_chars {
        return input.to_owned();
    }
    let mut out: String = input.chars().take(max_chars.saturating_sub(1)).collect();
    if ellipsis {
        out.push('?');
    }
    out
}

/// Removes a UTF-8 BOM and normalizes CRLF to LF. This is the exact byte form
/// that gets hashed and parsed (docs/04 ?4.4).
#[must_use]
pub fn normalize_file_bytes(input: &[u8]) -> Vec<u8> {
    let without_bom = input.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(input);
    let mut out = Vec::with_capacity(without_bom.len());
    let mut index = 0;
    while index < without_bom.len() {
        if without_bom[index] == b'\r' {
            if without_bom.get(index + 1) == Some(&b'\n') {
                out.push(b'\n');
                index += 2;
                continue;
            }
            out.push(b'\n');
            index += 1;
            continue;
        }
        out.push(without_bom[index]);
        index += 1;
    }
    out
}

/// `blob_hash = BLAKE3(normalized_file_bytes)` (no domain prefix).
#[must_use]
pub fn blob_hash(normalized: &[u8]) -> Digest {
    Digest::from_hex_unchecked(blake3::hash(normalized).to_hex().to_string())
}

/// `body_hash = BLAKE3("body\0" || token_stream)`.
#[must_use]
pub fn body_hash(token_stream: &str) -> Digest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"body\0");
    hasher.update(token_stream.as_bytes());
    Digest::from_hex_unchecked(hasher.finalize().to_hex().to_string())
}

/// `symbol_hash` per docs/21 ?2. `body_stream` is the normalized token stream
/// produced by the parser; `content_version` bumps when extraction changes
/// meaning.
#[must_use]
pub fn symbol_hash(
    language: Language,
    kind: SymbolKind,
    qualified_name: &str,
    signature: &str,
    docstring: Option<&str>,
    body_stream: &str,
    content_version: u32,
) -> crate::domain::SymbolHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"symbol\0");
    hasher.update(language.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(qualified_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(signature.as_bytes());
    hasher.update(b"\0");
    hasher.update(docstring.unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(body_stream.as_bytes());
    hasher.update(b"\0");
    hasher.update(content_version.to_le_bytes().as_slice());
    crate::domain::SymbolHash::from_hex_unchecked(hasher.finalize().to_hex().to_string())
}

/// `embedding_key = BLAKE3("embed\0" || template_version || "\0" || text)`.
#[must_use]
pub fn embedding_key(template_version: u32, text: &str) -> EmbeddingKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"embed\0");
    hasher.update(template_version.to_le_bytes().as_slice());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
    EmbeddingKey::from_hex_unchecked(hasher.finalize().to_hex().to_string())
}

/// `config_fingerprint` covers everything that invalidates stored vectors.
pub fn config_fingerprint(
    model_id: &str,
    revision: &str,
    dimensions: usize,
    template_version: u32,
    normalization_mode: &str,
) -> Result<crate::domain::ConfigFingerprint> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"config\0");
    hasher.update(model_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(revision.as_bytes());
    hasher.update(b"\0");
    hasher.update(dimensions.to_le_bytes().as_slice());
    hasher.update(b"\0");
    hasher.update(template_version.to_le_bytes().as_slice());
    hasher.update(b"\0");
    hasher.update(normalization_mode.as_bytes());
    crate::domain::ConfigFingerprint::parse(hasher.finalize().to_hex().to_string())
}

/// `worktree_fingerprint` over a manifest sorted by path then hash.
#[must_use]
pub fn manifest_fingerprint(entries: &[(RepoPath, Digest)]) -> Digest {
    let mut sorted: Vec<&(RepoPath, Digest)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"manifest\0");
    for (path, hash) in sorted {
        hasher.update(path.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_str().as_bytes());
        hasher.update(b"\0");
    }
    Digest::from_hex_unchecked(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SymbolKind;

    fn symbol(code: &str, doc: Option<&str>) -> SymbolDef {
        SymbolDef {
            symbol_hash: crate::domain::SymbolHash::from_hex_unchecked("0".repeat(64)),
            language: Language::Rust,
            name: "authenticate".into(),
            qualified_name: "authenticate".into(),
            kind: SymbolKind::Function,
            signature: "fn authenticate(req: Request) -> Result<Claims, AuthError>".into(),
            docstring: doc.map(str::to_owned),
            code_snippet: code.into(),
            body_hash: Digest::from_hex_unchecked("1".repeat(64)),
        }
    }

    #[test]
    fn bom_and_crlf_are_normalized() {
        let input = b"\xEF\xBB\xBFfn a() {\r\n    b();\r\n}\r\n";
        let normalized = normalize_file_bytes(input);
        assert_eq!(
            String::from_utf8(normalized).unwrap(),
            "fn a() {\n    b();\n}\n"
        );
    }

    #[test]
    fn blob_hash_is_stable_across_line_endings() {
        let lf = normalize_file_bytes(b"a\nb\n");
        let crlf = normalize_file_bytes(b"a\r\nb\r\n");
        assert_eq!(blob_hash(&lf), blob_hash(&crlf));
    }

    #[test]
    fn body_hash_ignores_whitespace_but_not_tokens() {
        let a = body_hash("identifier\u{1f}a\u{1f}+\u{1f}1");
        let b = body_hash("identifier\u{1f}a\u{1f}+\u{1f}1");
        let c = body_hash("identifier\u{1f}a\u{1f}-\u{1f}1");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn template_omits_empty_imports_and_doc() {
        let path = RepoPath::parse("src/auth.rs").unwrap();
        let template = EmbeddingTemplate::render(&path, "", &symbol("fn a() {}", None));
        assert!(!template.text().contains("imports:"));
        assert!(!template.text().contains("doc:"));
        assert!(template
            .text()
            .starts_with("path: src/auth.rs\nsymbol: fn authenticate"));
    }

    #[test]
    fn template_includes_doc_and_imports_when_present() {
        let path = RepoPath::parse("src/auth.rs").unwrap();
        let template = EmbeddingTemplate::render(
            &path,
            "use axum::http::Request;",
            &symbol("fn a() {}", Some("Memeriksa token")),
        );
        assert!(template
            .text()
            .contains("imports: use axum::http::Request;"));
        assert!(template.text().contains("doc: Memeriksa token"));
    }

    #[test]
    fn template_truncates_code_to_token_budget() {
        let path = RepoPath::parse("src/auth.rs").unwrap();
        let code = (0..80)
            .map(|i| format!("    let value_{i} = compute({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let count = |text: &str| text.chars().count().div_ceil(4);
        let template = EmbeddingTemplate::render_with_budget(
            &path,
            "",
            &symbol(&format!("fn big() {{\n{code}\n}}"), None),
            Some(40),
            Some(&count),
        );
        assert!(
            count(template.text()) <= 40,
            "{} tokens",
            count(template.text())
        );
        assert!(template.truncated_code());
    }

    #[test]
    fn embedding_key_changes_with_text_and_version() {
        let a = embedding_key(1, "hello");
        let b = embedding_key(1, "hello");
        let c = embedding_key(2, "hello");
        let d = embedding_key(1, "hello2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn symbol_hash_includes_docstring_and_signature() {
        let base = symbol_hash(
            Language::Rust,
            SymbolKind::Function,
            "f",
            "fn f()",
            None,
            "x",
            1,
        );
        let with_doc = symbol_hash(
            Language::Rust,
            SymbolKind::Function,
            "f",
            "fn f()",
            Some("doc"),
            "x",
            1,
        );
        let with_sig = symbol_hash(
            Language::Rust,
            SymbolKind::Function,
            "f",
            "fn f(a: i32)",
            None,
            "x",
            1,
        );
        assert_ne!(base, with_doc);
        assert_ne!(base, with_sig);
    }
}
