//! Language definitions, the registry and file-type detection.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use tree_sitter::Query;

use crate::grammars::{Spec, SPECS};
use crate::names::highlight_id_for_capture;

/// A compiled highlight query with its capture-index → highlight-id table.
pub(crate) struct HighlightQuery {
    pub(crate) query: Query,
    pub(crate) ids: Vec<Option<u16>>,
}

/// A compiled injection query.
pub(crate) struct InjectionQuery {
    pub(crate) query: Query,
    pub(crate) content_capture: Option<u32>,
    pub(crate) language_capture: Option<u32>,
}

/// A compiled outline query.
pub(crate) struct OutlineQuery {
    pub(crate) query: Query,
}

/// A compiled tags query.
pub(crate) struct TagsQuery {
    pub(crate) query: Query,
}

type Compiled<T> = Result<Option<T>, String>;

#[derive(Default)]
struct Queries {
    highlights: OnceLock<Compiled<HighlightQuery>>,
    injections: OnceLock<Compiled<InjectionQuery>>,
    outline: OnceLock<Compiled<OutlineQuery>>,
    tags: OnceLock<Compiled<TagsQuery>>,
}

/// A supported language: metadata plus its grammar and (lazily compiled) queries.
pub struct Language {
    /// Display name, also the key for [`LanguageRegistry::by_name`] ("Rust", "TSX", "C++").
    pub name: String,
    /// LSP `languageId` ("rust", "typescriptreact", "cpp", ...).
    pub lsp_id: String,
    /// Line comment prefix, if the language has one ("//", "#", "--").
    pub line_comment: Option<String>,
    /// Block comment delimiters, if the language has them.
    pub block_comment: Option<(String, String)>,
    /// File extensions (lowercase, no dot) detected as this language.
    pub extensions: Vec<String>,
    /// Alternative names (markdown fence info strings, etc.).
    pub aliases: Vec<String>,
    pub(crate) spec: &'static Spec,
    pub(crate) grammar: tree_sitter::Language,
    queries: Queries,
}

impl std::fmt::Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Language")
            .field("name", &self.name)
            .field("lsp_id", &self.lsp_id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Language {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

fn join(parts: &[&str]) -> String {
    parts.join("\n")
}

impl Language {
    fn from_spec(spec: &'static Spec) -> Language {
        Language {
            name: spec.name.to_string(),
            lsp_id: spec.lsp_id.to_string(),
            line_comment: spec.line_comment.map(str::to_string),
            block_comment: spec
                .block_comment
                .map(|(a, b)| (a.to_string(), b.to_string())),
            extensions: spec.extensions.iter().map(|s| s.to_string()).collect(),
            aliases: spec.aliases.iter().map(|s| s.to_string()).collect(),
            spec,
            grammar: (spec.grammar)(),
            queries: Queries::default(),
        }
    }

    pub(crate) fn highlight_query(&self) -> Option<&HighlightQuery> {
        self.queries
            .highlights
            .get_or_init(|| {
                if spec_is_empty(self.spec.highlights) {
                    return Ok(None);
                }
                let src = join(self.spec.highlights);
                let query = Query::new(&self.grammar, &src).map_err(|e| e.to_string())?;
                let ids = query
                    .capture_names()
                    .iter()
                    .map(|n| {
                        if *n == "none" {
                            Some(crate::highlight::CLEAR_ID)
                        } else {
                            highlight_id_for_capture(n).map(|id| id.0)
                        }
                    })
                    .collect();
                Ok(Some(HighlightQuery { query, ids }))
            })
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
    }

    pub(crate) fn injection_query(&self) -> Option<&InjectionQuery> {
        self.queries
            .injections
            .get_or_init(|| {
                if self.spec.injections.trim().is_empty() {
                    return Ok(None);
                }
                let query =
                    Query::new(&self.grammar, self.spec.injections).map_err(|e| e.to_string())?;
                let content_capture = query.capture_index_for_name("injection.content");
                let language_capture = query.capture_index_for_name("injection.language");
                Ok(Some(InjectionQuery {
                    query,
                    content_capture,
                    language_capture,
                }))
            })
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
    }

    pub(crate) fn outline_query(&self) -> Option<&OutlineQuery> {
        self.queries
            .outline
            .get_or_init(|| {
                if self.spec.outline.trim().is_empty() {
                    return Ok(None);
                }
                let query =
                    Query::new(&self.grammar, self.spec.outline).map_err(|e| e.to_string())?;
                Ok(Some(OutlineQuery { query }))
            })
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
    }

    pub(crate) fn tags_query(&self) -> Option<&TagsQuery> {
        self.queries
            .tags
            .get_or_init(|| {
                if spec_is_empty(self.spec.tags) {
                    return Ok(None);
                }
                let src = join(self.spec.tags);
                let query = Query::new(&self.grammar, &src).map_err(|e| e.to_string())?;
                Ok(Some(TagsQuery { query }))
            })
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
    }

    /// Compile every query of this language and report failures (empty when all
    /// queries are valid). Used by tests; the app never needs it.
    pub fn query_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let _ = self.highlight_query();
        let _ = self.injection_query();
        let _ = self.outline_query();
        let _ = self.tags_query();
        let mut note = |what: &str, e: Option<&String>| {
            if let Some(e) = e {
                errors.push(format!("{} {what}: {e}", self.name));
            }
        };
        note(
            "highlights",
            self.queries.highlights.get().and_then(|r| r.as_ref().err()),
        );
        note(
            "injections",
            self.queries.injections.get().and_then(|r| r.as_ref().err()),
        );
        note(
            "outline",
            self.queries.outline.get().and_then(|r| r.as_ref().err()),
        );
        note(
            "tags",
            self.queries.tags.get().and_then(|r| r.as_ref().err()),
        );
        errors
    }

    /// Extra node kinds (beyond the generic heuristics) that can fold.
    pub(crate) fn fold_kinds(&self) -> &'static [&'static str] {
        self.spec.fold_kinds
    }

    /// True for helper languages that are not user-selectable (markdown-inline).
    pub fn is_hidden(&self) -> bool {
        self.spec.hidden
    }
}

fn spec_is_empty(parts: &[&str]) -> bool {
    parts.iter().all(|p| p.trim().is_empty())
}

/// All built-in languages and file-type detection.
///
/// Cloning and `new()` are cheap: the language table (with its lazily
/// compiled queries) is process-global and shared.
#[derive(Clone)]
pub struct LanguageRegistry {
    languages: Arc<Vec<Arc<Language>>>,
}

static GLOBAL: OnceLock<LanguageRegistry> = OnceLock::new();

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageRegistry {
    /// The registry of built-in languages.
    pub fn new() -> Self {
        GLOBAL
            .get_or_init(|| LanguageRegistry {
                languages: Arc::new(
                    SPECS
                        .iter()
                        .map(|s| Arc::new(Language::from_spec(s)))
                        .collect(),
                ),
            })
            .clone()
    }

    /// Detect the language of a file by exact name, extension, then shebang /
    /// first line. `first_line` is the text of the first line, if available.
    pub fn language_for_path(
        &self,
        path: &Path,
        first_line: Option<&str>,
    ) -> Option<Arc<Language>> {
        let file_name = path.file_name().and_then(|n| n.to_str());
        if let Some(name) = file_name {
            let lower = name.to_ascii_lowercase();
            if let Some(l) = self.by_file_name(&lower, path) {
                return Some(l);
            }
            if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e) {
                if let Some(l) = self.by_extension(ext) {
                    return Some(l);
                }
            }
        }
        first_line.and_then(|l| self.by_first_line(l))
    }

    /// Language for a file extension (without the dot, case-insensitive).
    pub fn by_extension(&self, ext: &str) -> Option<Arc<Language>> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        self.languages
            .iter()
            .find(|l| l.extensions.contains(&ext))
            .cloned()
    }

    fn by_file_name(&self, lower_name: &str, path: &Path) -> Option<Arc<Language>> {
        if let Some(l) = self.languages.iter().find(|l| {
            l.spec
                .file_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(lower_name))
        }) {
            return Some(l.clone());
        }
        // JSON with comments: tsconfig.*.json, .eslintrc*, VS Code settings, workspaces.
        let is_json = lower_name.ends_with(".json");
        let jsonc_pattern = (is_json
            && (lower_name.starts_with("tsconfig.")
                || lower_name.starts_with("jsconfig.")
                || lower_name.starts_with(".eslintrc")
                || lower_name.starts_with(".babelrc")
                || lower_name.starts_with("tslint.")))
            || lower_name.starts_with(".eslintrc")
            || lower_name.ends_with(".code-workspace")
            || (is_json
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .is_some_and(|d| matches!(d, ".vscode" | ".devcontainer" | ".zed")));
        if jsonc_pattern {
            return self.by_name("JSONC");
        }
        // Bash dotfiles: .bash_history excluded, .bashrc-like variants included.
        if lower_name.starts_with(".bash") && !lower_name.contains("history") {
            return self.by_name("Bash");
        }
        if lower_name.starts_with(".zsh") && !lower_name.contains("history") {
            return self.by_name("Bash");
        }
        None
    }

    fn by_first_line(&self, line: &str) -> Option<Arc<Language>> {
        let line = line.trim_start_matches('\u{feff}').trim_end();
        if let Some(rest) = line.strip_prefix("#!") {
            let interpreter = shebang_interpreter(rest)?;
            return self.by_interpreter(&interpreter);
        }
        let lower = line.trim_start().to_ascii_lowercase();
        if lower.starts_with("<?php") {
            return self.by_name("PHP");
        }
        if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
            return self.by_name("HTML");
        }
        None
    }

    fn by_interpreter(&self, interpreter: &str) -> Option<Arc<Language>> {
        let base = interpreter.rsplit('/').next().unwrap_or(interpreter);
        let name = match base {
            "sh" | "bash" | "zsh" | "ksh" | "dash" | "ash" | "mksh" | "busybox" => "Bash",
            "node" | "nodejs" | "deno" | "bun" => "JavaScript",
            "ts-node" | "tsx" => "TypeScript",
            "ruby" | "jruby" | "rake" => "Ruby",
            "php" => "PHP",
            _ if base.starts_with("python") || base == "pypy" || base == "pypy3" => "Python",
            _ if base.starts_with("ruby") => "Ruby",
            _ => return None,
        };
        self.by_name(name)
    }

    /// Look up a language by display name, LSP id or alias (case-insensitive).
    /// Also resolves markdown fence info strings like `rust`, `ts`, `sh`, `yml`.
    pub fn by_name(&self, name: &str) -> Option<Arc<Language>> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        self.languages
            .iter()
            .find(|l| {
                l.name.eq_ignore_ascii_case(name)
                    || l.lsp_id.eq_ignore_ascii_case(name)
                    || l.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
            })
            .cloned()
    }

    /// User-visible languages, in a stable order.
    pub fn languages(&self) -> Vec<Arc<Language>> {
        self.languages
            .iter()
            .filter(|l| !l.is_hidden())
            .cloned()
            .collect()
    }
}

/// Extract the interpreter from the text after `#!` (handles `env` and `env -S`).
fn shebang_interpreter(rest: &str) -> Option<String> {
    let mut parts = rest.split_whitespace();
    let first = parts.next()?;
    if first.rsplit('/').next() == Some("env") {
        for p in parts {
            if p.starts_with('-') || p.contains('=') {
                continue;
            }
            return Some(p.to_string());
        }
        None
    } else {
        Some(first.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(path: &str, first: Option<&str>) -> Option<String> {
        LanguageRegistry::new()
            .language_for_path(Path::new(path), first)
            .map(|l| l.name.clone())
    }

    #[test]
    fn detects_by_extension() {
        assert_eq!(detect("src/main.rs", None).as_deref(), Some("Rust"));
        assert_eq!(detect("a/b/App.tsx", None).as_deref(), Some("TSX"));
        assert_eq!(detect("a.ts", None).as_deref(), Some("TypeScript"));
        assert_eq!(detect("a.d.ts", None).as_deref(), Some("TypeScript"));
        assert_eq!(detect("a.JSX", None).as_deref(), Some("JavaScript"));
        assert_eq!(detect("a.mjs", None).as_deref(), Some("JavaScript"));
        assert_eq!(detect("x.hpp", None).as_deref(), Some("C++"));
        assert_eq!(detect("x.h", None).as_deref(), Some("C"));
        assert_eq!(detect("Program.cs", None).as_deref(), Some("C#"));
        assert_eq!(detect("a.kts", None).as_deref(), Some("Kotlin"));
        assert_eq!(detect("README.md", None).as_deref(), Some("Markdown"));
        assert_eq!(detect("query.sql", None).as_deref(), Some("SQL"));
        assert_eq!(detect("a.yml", None).as_deref(), Some("YAML"));
    }

    #[test]
    fn detects_by_file_name() {
        assert_eq!(detect("Cargo.lock", None).as_deref(), Some("TOML"));
        assert_eq!(detect("/home/u/.bashrc", None).as_deref(), Some("Bash"));
        assert_eq!(detect(".zshrc", None).as_deref(), Some("Bash"));
        assert_eq!(detect(".bash_profile", None).as_deref(), Some("Bash"));
        assert_eq!(detect("tsconfig.json", None).as_deref(), Some("JSONC"));
        assert_eq!(
            detect("tsconfig.build.json", None).as_deref(),
            Some("JSONC")
        );
        assert_eq!(detect("package.json", None).as_deref(), Some("JSON"));
        assert_eq!(
            detect(".vscode/settings.json", None).as_deref(),
            Some("JSONC")
        );
        assert_eq!(detect("Gemfile", None).as_deref(), Some("Ruby"));
        assert_eq!(detect("Rakefile", None).as_deref(), Some("Ruby"));
        assert_eq!(detect("PKGBUILD", None).as_deref(), Some("Bash"));
    }

    #[test]
    fn detects_by_shebang_and_first_line() {
        assert_eq!(
            detect("script", Some("#!/bin/bash")).as_deref(),
            Some("Bash")
        );
        assert_eq!(
            detect("script", Some("#!/usr/bin/env python3")).as_deref(),
            Some("Python")
        );
        assert_eq!(
            detect("script", Some("#!/usr/bin/env -S node --flag")).as_deref(),
            Some("JavaScript")
        );
        assert_eq!(
            detect("tool", Some("#!/usr/bin/ruby -w")).as_deref(),
            Some("Ruby")
        );
        assert_eq!(
            detect("index", Some("<?php echo 1;")).as_deref(),
            Some("PHP")
        );
        assert_eq!(
            detect("page", Some("<!DOCTYPE html>")).as_deref(),
            Some("HTML")
        );
        assert_eq!(detect("noext", Some("hello world")), None);
        assert_eq!(detect("noext", None), None);
        // extension wins over a misleading first line
        assert_eq!(detect("a.py", Some("#!/bin/sh")).as_deref(), Some("Python"));
    }

    #[test]
    fn by_name_resolves_aliases() {
        let reg = LanguageRegistry::new();
        for (n, want) in [
            ("rust", "Rust"),
            ("RS", "Rust"),
            ("ts", "TypeScript"),
            ("tsx", "TSX"),
            ("js", "JavaScript"),
            ("jsx", "JavaScript"),
            ("py", "Python"),
            ("sh", "Bash"),
            ("shell", "Bash"),
            ("yml", "YAML"),
            ("c++", "C++"),
            ("cpp", "C++"),
            ("csharp", "C#"),
            ("cs", "C#"),
            ("golang", "Go"),
            ("md", "Markdown"),
            ("markdown_inline", "Markdown Inline"),
            ("postgresql", "SQL"),
        ] {
            assert_eq!(
                reg.by_name(n).map(|l| l.name.clone()).as_deref(),
                Some(want),
                "{n}"
            );
        }
        assert!(reg.by_name("klingon").is_none());
    }

    #[test]
    fn registry_lists_languages() {
        let reg = LanguageRegistry::new();
        let langs = reg.languages();
        assert!(langs.len() >= 20);
        assert!(langs.iter().all(|l| !l.is_hidden()));
    }
}
