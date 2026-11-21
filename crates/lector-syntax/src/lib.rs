//! Tree-sitter based syntax services for Lector: language registry and
//! detection, highlighting (with injections), outline, folds, bracket matching,
//! tags and a workspace symbol index.
//!
//! All operations read text through [`TextInput`] (implemented for
//! `lector_text::Text`, `str` and `String`), never copying whole files.
//! Always pass a [`Syntax`] the same text it was parsed from.
//!
//! # Languages
//!
//! `Language::name` values (use these in `lector_lsp::ServerConfig::languages`): Rust,
//! TypeScript, TSX, JavaScript (also `.jsx`), Python, Go, Java, Kotlin, Swift, C, C++, C#,
//! Ruby, PHP, Bash, HTML, CSS, JSON, JSONC, YAML, TOML, Markdown, SQL (plus the internal
//! "Markdown Inline" used by injections). [`LanguageRegistry::language_for_path`] detects by
//! exact file name (`Cargo.lock`, `.bashrc`, `Gemfile`, ...), name patterns (`tsconfig.*.json`
//! and JSON under `.vscode/` are JSONC), extension, then shebang / first line;
//! [`LanguageRegistry::by_name`] also accepts LSP ids and aliases (`rs`, `ts`, `sh`, `yml`).
//! Highlights follow injections (Markdown fences by info string, Markdown inline, HTML
//! `<script>` / `<style>`). Grammar and query provenance and licences:
//! `crates/lector-syntax/GRAMMARS.md`.
//!
//! # Folds and symbols
//!
//! In [`Syntax::fold_ranges`] the header row stays visible and rows `start + 1 .. end` hide.
//! A region that ends in a closer on its own row (`}`, `]`, `)`, `end`, `fi`, `</tag>`) ends
//! at that row so the closer stays visible; otherwise (Python blocks, Markdown sections) `end`
//! is one past the last row. A body that starts on its own line (Allman braces, Python/YAML
//! blocks) uses the previous non-blank row as header. [`OutlineItem::range`] covers the whole
//! item and `name_range` the identifier; a [`Tag`]'s range is the name identifier, and
//! definition tags merge the grammar's tags query with outline items so kinds agree
//! (Method vs Function, Struct vs Class). Parsing refuses texts over [`MAX_PARSE_BYTES`].

#![forbid(unsafe_code)]

mod error;
mod grammars;
mod highlight;
mod input;
mod language;
mod names;
mod outline;
mod structure;
mod symbols;
mod syntax;
mod tags;
mod walk;

pub use error::{Result, SyntaxError};
pub use highlight::HighlightSpan;
pub use input::{Point, TextInput};
pub use language::{Language, LanguageRegistry};
pub use names::{highlight_id_for_capture, HighlightId, HIGHLIGHT_NAMES};
pub use outline::{OutlineItem, SymbolKind};
pub use symbols::{SymbolIndex, SymbolLocation};
pub use syntax::{NodeInfo, Syntax, SyntaxEdit, MAX_PARSE_BYTES};
pub use tags::Tag;
pub use walk::TreeNode;

#[cfg(test)]
mod query_tests {
    use super::*;

    #[test]
    fn all_queries_compile() {
        let mut errors = Vec::new();
        for lang in LanguageRegistry::new().languages() {
            errors.extend(lang.query_errors());
        }
        assert!(errors.is_empty(), "query errors:\n{}", errors.join("\n"));
    }
}
