//! Dev tool: print the s-expression of a file's tree.
use lector_syntax::{LanguageRegistry, Syntax};
fn main() {
    let path = std::env::args().nth(1).unwrap_or_default();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let reg = LanguageRegistry::new();
    let Some(lang) = reg.language_for_path(std::path::Path::new(&path), text.lines().next()) else {
        return;
    };
    let syntax = Syntax::parse(&lang, text.as_str()).unwrap();
    println!("{}", syntax.debug_sexp());
}
