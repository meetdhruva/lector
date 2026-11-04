//! Tree-sitter based syntax services for Lector: language registry and
//! detection, highlighting (with injections), outline, folds, bracket matching,
//! tags and a workspace symbol index.
//!
//! All operations read text through [`TextInput`] (implemented for
//! `lector_text::Text`, `str` and `String`), never copying whole files.

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

pub use error::{Result, SyntaxError};
pub use highlight::HighlightSpan;
pub use input::{Point, TextInput};
pub use language::{Language, LanguageRegistry};
pub use names::{highlight_id_for_capture, HighlightId, HIGHLIGHT_NAMES};
pub use outline::{OutlineItem, SymbolKind};
pub use symbols::{SymbolIndex, SymbolLocation};
pub use syntax::{NodeInfo, Syntax, SyntaxEdit, MAX_PARSE_BYTES};
pub use tags::Tag;

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
