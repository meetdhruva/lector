//! Behavioural tests for the public API using in-memory strings.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use lector_syntax::{
    highlight_id_for_capture, HighlightId, Language, LanguageRegistry, SymbolIndex, SymbolKind,
    Syntax, SyntaxError, TextInput, MAX_PARSE_BYTES,
};

fn lang(name: &str) -> Arc<Language> {
    LanguageRegistry::new()
        .by_name(name)
        .unwrap_or_else(|| panic!("no language {name}"))
}

fn parse(name: &str, src: &str) -> Syntax {
    Syntax::parse(&lang(name), src).expect("parse")
}

fn name_of(id: HighlightId) -> &'static str {
    id.name()
}

#[test]
fn highlights_are_sorted_non_overlapping_and_clipped() {
    let src = include_str!("samples/sample.rs");
    let syn = parse("rust", src);
    let spans = syn.highlights(src, 0..src.len());
    assert!(spans.len() > 100);
    for w in spans.windows(2) {
        assert!(
            w[0].range.end <= w[1].range.start,
            "overlap: {:?} {:?}",
            w[0],
            w[1]
        );
    }
    // Window clipping.
    let window = 100..400;
    let part = syn.highlights(src, window.clone());
    assert!(!part.is_empty());
    assert!(part
        .iter()
        .all(|s| s.range.start >= window.start && s.range.end <= window.end));
    // Out-of-range / empty requests are harmless.
    assert!(syn.highlights(src, 10..10).is_empty());
    assert!(!syn.highlights(src, 0..usize::MAX).is_empty());
    assert!(syn.highlights(src, src.len() + 5..src.len() + 9).is_empty());
}

#[test]
fn innermost_capture_wins() {
    let src = "fn main() { let x = 1; }\n";
    let syn = parse("rust", src);
    let spans = syn.highlights(src, 0..src.len());
    let at = |needle: &str| {
        let start = src.find(needle).unwrap();
        spans
            .iter()
            .find(|s| s.range.start <= start && start < s.range.end)
            .map(|s| name_of(s.id))
    };
    assert_eq!(at("fn"), Some("keyword"));
    assert_eq!(at("main"), Some("function"));
    assert_eq!(at("let"), Some("keyword"));
    assert_eq!(at("1"), Some("number"));
}

#[test]
fn spans_never_end_with_a_newline() {
    let src = "// comment\nfn a() {}\n/// doc\nfn b() {}\n";
    let syn = parse("rust", src);
    for s in syn.highlights(src, 0..src.len()) {
        assert!(
            !src[s.range.clone()].ends_with('\n'),
            "{:?}",
            &src[s.range.clone()]
        );
    }
}

#[test]
fn markdown_fences_use_the_info_string_language() {
    let src = "# T\n\n```python\ndef f(x):\n    return x\n```\n\n```nosuchlang\nfn x\n```\n";
    let syn = parse("markdown", src);
    let spans = syn.highlights(src, 0..src.len());
    let find = |needle: &str| {
        let start = src.find(needle).unwrap();
        spans
            .iter()
            .find(|s| s.range.start <= start && start < s.range.end)
            .map(|s| name_of(s.id))
    };
    assert_eq!(find("def"), Some("keyword"));
    assert_eq!(find("return"), Some("keyword"));
    // Unknown languages keep the host (markup.raw) styling, not a crash.
    assert_eq!(find("fn x"), Some("markup.raw"));
    // Windowing into the middle of an injected block still highlights.
    let start = src.find("return").unwrap();
    let part = syn.highlights(src, start..start + 6);
    assert_eq!(part.len(), 1);
}

#[test]
fn markdown_inline_and_html_script_injection() {
    let md = "plain *emph* and **strong** `code`\n";
    let syn = parse("markdown", md);
    let names: Vec<_> = syn
        .highlights(md, 0..md.len())
        .iter()
        .map(|s| name_of(s.id))
        .collect();
    assert!(names.contains(&"markup.italic"));
    assert!(names.contains(&"markup.bold"));
    assert!(names.contains(&"markup.raw"));

    let html = "<script>const a = 1;</script><style>p { color: red }</style>";
    let syn = parse("html", html);
    let names: Vec<_> = syn
        .highlights(html, 0..html.len())
        .iter()
        .map(|s| name_of(s.id))
        .collect();
    assert!(names.contains(&"keyword"), "{names:?}");
    assert!(names.contains(&"property"), "{names:?}");
}

#[test]
fn outline_and_enclosing_scopes() {
    let src = "mod m {\n    struct S;\n    impl S {\n        fn go(&self) {\n            let x = 1;\n        }\n    }\n}\n";
    let syn = parse("rust", src);
    let outline = syn.outline(src);
    let kinds: Vec<_> = outline
        .iter()
        .map(|i| (i.kind, i.name.as_str(), i.depth))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (SymbolKind::Module, "m", 0),
            (SymbolKind::Struct, "S", 1),
            (SymbolKind::Other, "impl S", 1),
            (SymbolKind::Method, "go", 2),
        ]
    );
    let go = &outline[3];
    assert_eq!(&src[go.name_range.clone()], "go");
    let at = src.find("let x").unwrap();
    let scopes = syn.enclosing_scopes(src, at);
    let names: Vec<_> = scopes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["m", "impl S", "go"]);
    // Outside everything.
    assert!(syn.enclosing_scopes(src, src.len()).is_empty());
    // Top of file, before any item contents.
    assert_eq!(syn.enclosing_scopes(src, 0).len(), 1);
}

#[test]
fn outline_items_are_in_document_order() {
    for file in [
        include_str!("samples/sample.rs"),
        include_str!("samples/sample.rs"),
    ] {
        let syn = parse("rust", file);
        let o = syn.outline(file);
        assert!(o.windows(2).all(|w| w[0].range.start <= w[1].range.start));
    }
}

#[test]
fn markdown_outline_nests_headings() {
    let src = "# A\n\ntext\n\n## B\n\n### C\n\n## D\n";
    let syn = parse("markdown", src);
    let items: Vec<_> = syn
        .outline(src)
        .iter()
        .map(|i| (i.name.clone(), i.depth))
        .collect();
    assert_eq!(
        items,
        vec![
            ("A".into(), 0),
            ("B".into(), 1),
            ("C".into(), 2),
            ("D".into(), 1)
        ]
    );
}

#[test]
fn fold_ranges_cover_blocks_and_keep_closers_visible() {
    let src = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n\nfn b() { 1 }\n\nstruct S {\n    f: u8,\n}\n";
    let syn = parse("rust", src);
    let folds = syn.fold_ranges(src);
    // Header row 0, closing brace on row 3 stays visible: hidden rows 1..3.
    assert!(folds.contains(&(0..3)), "{folds:?}");
    assert!(folds.contains(&(7..9)), "{folds:?}");
    // Single-line blocks never fold.
    assert!(folds.iter().all(|r| r.start != 5));
    // Sorted by start, one per start row.
    assert!(folds.windows(2).all(|w| w[0].start < w[1].start));

    let py = "def f():\n    x = 1\n    return x\n\nclass C:\n    pass\n";
    let syn = parse("python", py);
    let folds = syn.fold_ranges(py);
    // Python has no closer: the whole body hides.
    assert!(folds.contains(&(0..3)), "{folds:?}");
    assert!(
        folds.contains(&(4..6)) || folds.iter().any(|r| r.start == 4) || folds.len() == 1,
        "{folds:?}"
    );
}

#[test]
fn allman_braces_use_the_previous_line_as_header() {
    let src = "class A\n{\n    void f()\n    {\n        int x = 1;\n    }\n}\n";
    let syn = parse("c#", src);
    let folds = syn.fold_ranges(src);
    assert!(folds.contains(&(0..6)), "{folds:?}");
    assert!(folds.contains(&(2..5)), "{folds:?}");
}

#[test]
fn bracket_pairs() {
    let src = "fn f(a: (u8, u8)) { [1, 2] }";
    let syn = parse("rust", src);
    let open = src.find('(').unwrap();
    let close = src.find("))").unwrap() + 1;
    let (a, b) = syn.bracket_pair(src, open).unwrap();
    assert_eq!((a.start, b.start), (open, close));
    // Cursor just after the closing bracket still matches it.
    let (a, b) = syn.bracket_pair(src, close + 1).unwrap();
    assert_eq!((a.start, b.start), (close, open));
    // Inner pair.
    let inner = src.find("(u8").unwrap();
    let (a, b) = syn.bracket_pair(src, inner).unwrap();
    assert_eq!(a.start, inner);
    assert_eq!(&src[b.clone()], ")");
    assert_eq!(b.start, src.find("u8)").unwrap() + 2);
    // Not next to a bracket.
    assert!(syn.bracket_pair(src, src.find(", u8").unwrap()).is_none());
    // Enclosing pair of the `a` identifier is the parameter list.
    let (eo, ec) = syn
        .enclosing_bracket_pair(src, src.find("a:").unwrap())
        .unwrap();
    assert_eq!((eo.start, ec.start), (open, close));
}

#[test]
fn brackets_inside_strings_are_ignored() {
    let src = "let s = \"(\"; let t = (1);";
    let syn = parse("rust", src);
    let in_string = src.find('(').unwrap();
    assert!(syn.bracket_pair(src, in_string).is_none());
    let real = src.rfind('(').unwrap();
    let (a, b) = syn.bracket_pair(src, real).unwrap();
    assert_eq!((a.start, b.start), (real, real + 2));
}

#[test]
fn node_at_finds_identifiers() {
    let src = "fn compute(value: u32) -> u32 { value + 1 }";
    let syn = parse("rust", src);
    let off = src.find("compute").unwrap() + 3;
    let n = syn.node_at(off).unwrap();
    assert_eq!(n.kind, "identifier");
    assert!(n.is_identifier());
    assert_eq!(&src[n.range.clone()], "compute");
    let ty = syn.node_at(src.find("u32").unwrap()).unwrap();
    assert_eq!(ty.kind, "primitive_type");
    assert!(!ty.is_identifier());
    // End-of-text offsets do not panic.
    assert!(syn.node_at(src.len()).is_some());
    assert!(syn.node_at(src.len() + 100).is_some());
}

#[test]
fn tags_have_definitions_and_references() {
    let src =
        "fn helper() {}\nfn main() {\n    helper();\n}\nstruct Thing;\nconst LIMIT: u8 = 3;\n";
    let syn = parse("rust", src);
    let tags = syn.tags(src);
    let defs: Vec<_> = tags
        .iter()
        .filter(|t| t.is_definition)
        .map(|t| (t.name.as_str(), t.kind))
        .collect();
    assert!(defs.contains(&("helper", SymbolKind::Function)), "{defs:?}");
    assert!(defs.contains(&("main", SymbolKind::Function)), "{defs:?}");
    assert!(defs.contains(&("Thing", SymbolKind::Struct)), "{defs:?}");
    assert!(defs.contains(&("LIMIT", SymbolKind::Constant)), "{defs:?}");
    assert!(tags.iter().any(|t| !t.is_definition && t.name == "helper"));
    for t in &tags {
        assert_eq!(&src[t.range.clone()], t.name);
    }
}

#[test]
fn symbol_index_insert_remove_query() {
    let src_a = "fn helper() {}\nfn main() { helper(); }\n";
    let src_b = "fn helper() {}\nfn other() { helper(); }\n";
    let (a, b) = (parse("rust", src_a), parse("rust", src_b));
    let index = SymbolIndex::new();
    let (pa, pb) = (PathBuf::from("a.rs"), PathBuf::from("b.rs"));
    index.insert_file(pa.clone(), src_a, a.tags(src_a));
    index.insert_file(pb.clone(), src_b, b.tags(src_b));
    assert_eq!(index.file_count(), 2);

    let defs = index.definitions("helper");
    assert_eq!(defs.len(), 2);
    assert_eq!(defs[0].path, pa);
    assert_eq!(defs[0].point.row, 0);
    assert_eq!(defs[0].kind, SymbolKind::Function);
    assert_eq!(index.references("helper").len(), 2);
    assert_eq!(index.references("helper")[0].point.row, 1);
    assert!(index.all_definitions().iter().any(|d| d.name == "other"));
    assert_eq!(index.definitions_in_file(&pb).len(), 2);

    // Re-inserting replaces; removing forgets.
    index.insert_file(pa.clone(), src_b, b.tags(src_b));
    assert_eq!(index.definitions("main").len(), 0);
    index.remove_file(&pb);
    assert_eq!(index.definitions("helper").len(), 1);
    assert_eq!(index.definitions("other").len(), 1);
    index.remove_file(&pa);
    assert!(index.all_definitions().is_empty());
    assert_eq!(index.file_count(), 0);
}

#[test]
fn symbol_index_is_thread_safe() {
    let index = Arc::new(SymbolIndex::new());
    let src = "fn f() {}\n";
    let tags = parse("rust", src).tags(src);
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let index = index.clone();
            let tags = tags.clone();
            std::thread::spawn(move || {
                for j in 0..50 {
                    let p = PathBuf::from(format!("f{i}_{j}.rs"));
                    index.insert_file(p.clone(), "fn f() {}\n", tags.clone());
                    let _ = index.definitions("f");
                    if j % 2 == 0 {
                        index.remove_file(&p);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    assert_eq!(index.file_count(), 100);
}

#[test]
fn refuses_files_over_the_limit() {
    let big = "a".repeat(MAX_PARSE_BYTES + 1);
    match Syntax::parse(&lang("rust"), big.as_str()) {
        Err(SyntaxError::TooLarge { len, max }) => {
            assert_eq!(len, MAX_PARSE_BYTES + 1);
            assert_eq!(max, MAX_PARSE_BYTES);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn parse_can_be_cancelled() {
    let src = include_str!("samples/sample.rs").repeat(200);
    let cancelled = AtomicBool::new(true);
    let r = Syntax::parse_cancellable(&lang("rust"), src.as_str(), &cancelled);
    assert!(matches!(r, Err(SyntaxError::ParseFailed(_))));
    let not = AtomicBool::new(false);
    assert!(Syntax::parse_cancellable(&lang("rust"), src.as_str(), &not).is_ok());
}

#[test]
fn parse_reads_through_chunks() {
    /// A text type that serves tiny chunks, to prove chunked reading works.
    struct Chunked(String);
    impl TextInput for Chunked {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn chunks_in_range(
            &self,
            range: std::ops::Range<usize>,
        ) -> Box<dyn Iterator<Item = &str> + '_> {
            let s = &self.0[range];
            let mut idx = 0;
            Box::new(std::iter::from_fn(move || {
                if idx >= s.len() {
                    return None;
                }
                let mut end = (idx + 7).min(s.len());
                while !s.is_char_boundary(end) {
                    end += 1;
                }
                let piece = &s[idx..end];
                idx = end;
                Some(piece)
            }))
        }
        fn text_for_range(&self, range: std::ops::Range<usize>) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.0[range])
        }
        fn offset_to_point(&self, offset: usize) -> lector_syntax::Point {
            self.0.as_str().offset_to_point(offset)
        }
    }
    let src = include_str!("samples/sample.rs");
    let chunked = Chunked(src.to_string());
    let a = Syntax::parse(&lang("rust"), &chunked).expect("chunked");
    let b = parse("rust", src);
    assert_eq!(a.debug_sexp(), b.debug_sexp());
    assert_eq!(
        a.highlights(&chunked, 0..src.len()),
        b.highlights(src, 0..src.len())
    );
    assert_eq!(a.outline(&chunked), b.outline(src));
}

#[test]
fn unicode_text_keeps_byte_offsets_consistent() {
    let src = "// héllo wörld ✓\nfn größe() -> &'static str { \"日本語\" }\n";
    let syn = parse("rust", src);
    for s in syn.highlights(src, 0..src.len()) {
        assert!(src.is_char_boundary(s.range.start) && src.is_char_boundary(s.range.end));
    }
    let o = syn.outline(src);
    assert_eq!(&src[o[0].name_range.clone()], "größe");
}

#[test]
fn empty_and_garbage_input_do_not_panic() {
    for name in [
        "rust", "markdown", "json", "yaml", "html", "sql", "python", "tsx",
    ] {
        for src in [
            "",
            "\n",
            "((((",
            "}}}}",
            "\u{0}\u{1}",
            "```",
            "<<<<",
            "'\"`",
        ] {
            let syn = parse(name, src);
            let _ = syn.highlights(src, 0..src.len());
            let _ = syn.outline(src);
            let _ = syn.fold_ranges(src);
            let _ = syn.tags(src);
            let _ = syn.bracket_pair(src, 0);
            let _ = syn.node_at(0);
        }
    }
}

#[test]
fn capture_mapping_is_exposed() {
    assert_eq!(
        highlight_id_for_capture("function.method").map(|i| i.name()),
        Some("function.method")
    );
}

#[test]
fn public_types_are_send_sync_clone() {
    fn assert_traits<T: Send + Sync>() {}
    fn assert_clone<T: Clone>() {}
    assert_traits::<Syntax>();
    assert_traits::<LanguageRegistry>();
    assert_traits::<Language>();
    assert_traits::<SymbolIndex>();
    assert_clone::<Syntax>();
    assert_clone::<LanguageRegistry>();
}

#[test]
fn syntax_can_be_shared_across_threads() {
    let src = include_str!("samples/sample.rs");
    let syn = parse("rust", src);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..20 {
                    assert!(!syn.highlights(src, 0..src.len()).is_empty());
                    assert!(!syn.outline(src).is_empty());
                    assert!(!syn.fold_ranges(src).is_empty());
                }
            });
        }
    });
}
