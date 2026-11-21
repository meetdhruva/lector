//! Tests of the lexer and the test-region classifier.

use super::*;

fn kinds(src: &str) -> Vec<(TokKind, String)> {
    let s = scan(src);
    s.tokens
        .iter()
        .map(|t| (t.kind, src[t.start..t.end].to_string()))
        .collect()
}

fn texts(src: &str) -> Vec<String> {
    kinds(src).into_iter().map(|(_, t)| t).collect()
}

/// Texts of the idents that are outside test regions.
fn prod_idents(src: &str) -> Vec<String> {
    let s = scan(src);
    (0..s.tokens.len())
        .filter(|i| !s.in_test[*i] && s.tokens[*i].kind == TokKind::Ident)
        .map(|i| s.text(src, i).to_string())
        .collect()
}

#[test]
fn line_comments_are_dropped() {
    let t = texts("a // thread::spawn(\nb /// doc thread::spawn\nc //! x\n");
    assert_eq!(t, ["a", "b", "c"]);
}

#[test]
fn nested_block_comments_are_dropped() {
    let t = texts("x /* a /* b */ still comment thread::spawn */ y /**/ z");
    assert_eq!(t, ["x", "y", "z"]);
    let s = scan("/* /* */ */ end");
    assert_eq!(s.comments.len(), 1);
    assert_eq!(s.tokens.len(), 1);
}

#[test]
fn block_comment_counts_lines() {
    let s = scan("a\n/* one\ntwo\nthree */\nb");
    assert_eq!(s.tokens[1].line, 5);
    assert_eq!((s.comments[0].start_line, s.comments[0].end_line), (2, 4));
}

#[test]
fn slashes_inside_strings_are_not_comments() {
    let k = kinds(r#"let u = "http://x /* y */"; z"#);
    assert!(k
        .iter()
        .any(|(kind, t)| *kind == TokKind::Str && t.contains("http://x")));
    assert_eq!(k.last().map(|(_, t)| t.as_str()), Some("z"));
}

#[test]
fn escaped_quote_does_not_end_string() {
    let k = kinds(r#"f("a\"b // c") g"#);
    assert_eq!(k.len(), 5, "{k:?}");
    assert_eq!(k[2].0, TokKind::Str);
    assert_eq!(k[4].1, "g");
}

#[test]
fn raw_strings_with_comment_markers_and_quotes() {
    let src = "let a = r#\"x // y \" z\"#; let b = r##\"p \"# q\"##; end";
    let k = kinds(src);
    let strs: Vec<&String> = k
        .iter()
        .filter(|(kind, _)| *kind == TokKind::Str)
        .map(|(_, t)| t)
        .collect();
    assert_eq!(strs.len(), 2, "{k:?}");
    assert_eq!(strs[0], "r#\"x // y \" z\"#");
    assert_eq!(strs[1], "r##\"p \"# q\"##");
    assert_eq!(k.last().map(|(_, t)| t.as_str()), Some("end"));
}

#[test]
fn byte_and_c_strings() {
    let k = kinds(r##"b"a\"b" br#"x"y"# c"z" cr"w" b'a' b'\'' tail"##);
    let got: Vec<TokKind> = k.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        got,
        [
            TokKind::Str,
            TokKind::Str,
            TokKind::Str,
            TokKind::Str,
            TokKind::Char,
            TokKind::Char,
            TokKind::Ident
        ],
        "{k:?}"
    );
}

#[test]
fn multi_line_raw_string_keeps_line_numbers() {
    let s = scan("a r\"l1\nl2\nl3\" b");
    assert_eq!(s.tokens[2].line, 3);
}

#[test]
fn lifetimes_versus_char_literals() {
    let k = kinds("fn f<'a, 'static_>(x: &'a str) -> char { 'x' }");
    let lifetimes: Vec<&str> = k
        .iter()
        .filter(|(kind, _)| *kind == TokKind::Lifetime)
        .map(|(_, t)| t.as_str())
        .collect();
    assert_eq!(lifetimes, ["'a", "'static_", "'a"]);
    assert!(k.contains(&(TokKind::Char, "'x'".to_string())));
}

#[test]
fn tricky_char_literals() {
    let k = kinds(r#"'"' '\'' '\\' '\u{1F600}' '_' 'é' x"#);
    let chars: Vec<&str> = k
        .iter()
        .filter(|(kind, _)| *kind == TokKind::Char)
        .map(|(_, t)| t.as_str())
        .collect();
    assert_eq!(
        chars,
        [r#"'"'"#, r"'\''", r"'\\'", r"'\u{1F600}'", "'_'", "'é'"]
    );
    assert_eq!(k.last().map(|(_, t)| t.as_str()), Some("x"));
}

#[test]
fn quote_char_literal_does_not_open_a_string() {
    let t = prod_idents("let q = '\"'; thread::spawn(f);");
    assert!(t.contains(&"spawn".to_string()));
}

#[test]
fn raw_identifiers_and_numbers() {
    assert_eq!(texts("r#type"), ["r#type"]);
    assert_eq!(texts("1..2"), ["1", ".", ".", "2"]);
    assert_eq!(texts("1.5e-3 + x"), ["1.5e-3", "+", "x"]);
    assert_eq!(texts("0xE+1"), ["0xE", "+", "1"]);
    assert_eq!(texts("v.0.len()"), ["v", ".", "0", ".", "len", "(", ")"]);
}

#[test]
fn merged_punctuation() {
    assert_eq!(
        texts("a::b -> c => d"),
        ["a", "::", "b", "->", "c", "=>", "d"]
    );
}

#[test]
fn cfg_test_mod_is_a_region_and_strings_do_not_confuse_braces() {
    let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let s = \"}\"; let c = '}'; /* } */ }\n    fn thread_spawn() {}\n}\nfn after() {}\n";
    let idents = prod_idents(src);
    assert!(idents.contains(&"a".to_string()));
    assert!(idents.contains(&"after".to_string()));
    assert!(!idents.contains(&"thread_spawn".to_string()));
    assert!(!idents.contains(&"tests".to_string()));
}

#[test]
fn test_fn_attributes() {
    let src = "#[test]\nfn one() {}\n#[gpui::test]\nasync fn two() {}\n#[tokio::test(flavor = \"x\")]\nfn three() {}\n#[should_panic]\n#[test]\nfn four() {}\nfn real() {}";
    let idents = prod_idents(src);
    for hidden in ["one", "two", "three", "four"] {
        assert!(!idents.contains(&hidden.to_string()), "{hidden}");
    }
    assert!(idents.contains(&"real".to_string()));
}

#[test]
fn cfg_predicates() {
    for (attr, is_test) in [
        ("#[cfg(test)]", true),
        ("#[cfg(all(test, feature = \"x\"))]", true),
        ("#[cfg(any(test, unix))]", false),
        ("#[cfg(any(test, all(test, unix)))]", true),
        ("#[cfg(not(test))]", false),
        ("#[cfg(feature = \"test\")]", false),
        ("#[cfg_attr(test, derive(Debug))]", false),
    ] {
        let src = format!("{attr}\nfn marker() {{}}\n");
        let in_test = !prod_idents(&src).contains(&"marker".to_string());
        assert_eq!(in_test, is_test, "{attr}");
    }
}

#[test]
fn stacked_attributes_before_the_item() {
    let src = "#[cfg(test)]\n#[allow(dead_code)]\n#[derive(Debug)]\nstruct Hidden { a: u8 }\nstruct Shown;";
    assert_eq!(prod_idents(src), ["struct", "Shown"]);
}

#[test]
fn out_of_line_test_module_is_reported() {
    let s = scan("#[cfg(test)]\nmod tests;\nmod real;\n#[cfg(test)]\npub(crate) mod support;\n");
    assert_eq!(s.test_mod_decls, ["tests", "support"]);
    let idents = prod_idents("#[cfg(test)]\nmod tests;\nmod real;\n");
    assert_eq!(idents, ["mod", "real"]);
}

#[test]
fn nested_inner_cfg_test_marks_only_its_module() {
    let src = "fn production() { std::thread::spawn(|| {}); }\nmod checks {\n    #![cfg(test)]\n    fn helper() {}\n}\nfn after() {}";
    let s = scan(src);
    assert!(!s.whole_file_test);
    let idents = prod_idents(src);
    assert!(idents.contains(&"production".to_string()));
    assert!(idents.contains(&"spawn".to_string()));
    assert!(idents.contains(&"after".to_string()));
    assert!(!idents.contains(&"helper".to_string()));
    // A file-level attribute after other inner attributes still marks the whole file.
    assert!(scan("#![allow(dead_code)]\n#![cfg(test)]\nmod a { fn x() {} }").whole_file_test);
}

#[test]
fn test_mod_declarations_inside_inline_modules_are_not_resolved_at_file_level() {
    let s = scan("mod inner {\n    #[cfg(test)]\n    mod fixtures;\n}\n#[cfg(test)]\nmod tests;");
    assert_eq!(s.test_mod_decls, ["tests"]);
}

#[test]
fn whole_file_test_marker() {
    assert!(scan("#![cfg(test)]\nfn x() {}").whole_file_test);
    assert!(!scan("#![forbid(unsafe_code)]\nfn x() {}").whole_file_test);
}

#[test]
fn statement_like_items_end_at_the_semicolon() {
    let src = "#[cfg(test)]\nuse foo::{a, b};\n#[cfg(test)]\nconst T: [u8; 3] = [1, 2, 3];\n#[cfg(test)]\nstatic S: Foo = Foo { x: 1 };\nfn live() {}";
    assert_eq!(prod_idents(src), ["fn", "live"]);
}

#[test]
fn test_items_inside_impl_blocks() {
    let src = "impl X {\n fn keep(&self) {}\n #[cfg(test)]\n fn hide(&self) { let _ = 1; }\n fn keep2(&self) {}\n}";
    let idents = prod_idents(src);
    assert!(idents.contains(&"keep".to_string()));
    assert!(idents.contains(&"keep2".to_string()));
    assert!(!idents.contains(&"hide".to_string()));
}

#[test]
fn where_clauses_and_generics_in_test_items() {
    let src =
        "#[cfg(test)]\nfn f<T>(x: [T; 2]) -> Result<(), E> where T: Fn() { body() }\nfn g() {}";
    assert_eq!(prod_idents(src), ["fn", "g"]);
}

#[test]
fn unterminated_input_never_panics_and_spans_stay_in_bounds() {
    let sample = "#[cfg(test)] mod t { fn f() { let s = r#\"a\"#; 'a' 'b b\"c /* /* \\ } ";
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut inputs: Vec<String> = (0..=sample.len())
        .filter(|i| sample.is_char_boundary(*i))
        .map(|i| sample[..i].to_string())
        .collect();
    for _ in 0..300 {
        let mut bytes = sample.as_bytes().to_vec();
        for _ in 0..4 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let pos = (seed as usize) % bytes.len();
            let alphabet = b"\"'#r/*{}()[]\\ \n;#!bc";
            bytes[pos] = alphabet[(seed >> 20) as usize % alphabet.len()];
        }
        inputs.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    for src in inputs {
        let s = scan(&src);
        let mut prev = 0;
        for t in &s.tokens {
            assert!(
                t.start >= prev && t.start < t.end && t.end <= src.len(),
                "{src:?}"
            );
            prev = t.end;
        }
        assert_eq!(s.in_test.len(), s.tokens.len());
    }
}

// ---- end of an outer-attribute region: one rule for every position -------------------

/// Identifiers that carry no information in the position table.
const NOISE: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "impl",
    "trait",
    "const",
    "static",
    "let",
    "use",
    "mod",
    "pub",
    "crate",
    "self",
    "mut",
    "match",
    "if",
    "else",
    "type",
    "where",
    "unsafe",
    "async",
    "extern",
    "move",
    "macro_rules",
    "u8",
    "u16",
    "u32",
    "bool",
    "cfg",
    "test",
    "all",
    "any",
    "not",
];

/// (production, test-only) identifiers of `src`, sorted and deduplicated, noise removed.
fn split_idents(src: &str) -> (Vec<String>, Vec<String>) {
    let s = scan(src);
    let (mut prod, mut test) = (Vec::new(), Vec::new());
    for i in 0..s.tokens.len() {
        if s.tokens[i].kind != TokKind::Ident || NOISE.contains(&s.text(src, i)) {
            continue;
        }
        let name = s.text(src, i).to_string();
        if s.in_test[i] {
            test.push(name);
        } else {
            prod.push(name);
        }
    }
    for v in [&mut prod, &mut test] {
        v.sort();
        v.dedup();
    }
    (prod, test)
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    v.sort();
    v
}

/// `(position, source, production idents, test-only idents)`.
#[allow(clippy::type_complexity)]
const POSITIONS: &[(&str, &str, &[&str], &[&str])] = &[
    (
        "reviewer example, verbatim",
        "struct S { #[cfg(test)] value: u8, } fn production() { std::thread::spawn(|| {}); }",
        &["S", "production", "std", "thread", "spawn"],
        &["value"],
    ),
    (
        "field with generic type containing commas",
        "struct S { a: u8, #[cfg(test)] m: HashMap<String, Vec<u8>>, b: u8 } fn after() {}",
        &["S", "a", "b", "after"],
        &["m", "HashMap", "String", "Vec"],
    ),
    (
        "field with nested generics and a path",
        "struct S { #[cfg(test)] m: std::collections::HashMap<K, (A, B)>, b: u8 } fn after() {}",
        &["S", "b", "after"],
        &["m", "std", "collections", "HashMap", "K", "A", "B"],
    ),
    (
        "last field without trailing comma",
        "struct S { a: u8, #[cfg(test)] m: Foo } fn after() {}",
        &["S", "a", "after"],
        &["m", "Foo"],
    ),
    (
        "first field",
        "struct S { #[cfg(test)] m: Foo, a: u8 } fn after() {}",
        &["S", "a", "after"],
        &["m", "Foo"],
    ),
    (
        "visibility on a field",
        "struct S { #[cfg(test)] pub(crate) hidden: u8, pub shown: u8 } fn after() {}",
        &["S", "shown", "after"],
        &["hidden"],
    ),
    (
        "tuple struct field",
        "struct T(#[cfg(test)] Hidden, Shown); fn after() {}",
        &["T", "Shown", "after"],
        &["Hidden"],
    ),
    (
        "tuple struct field with generics",
        "struct T(#[cfg(test)] Map<A, B>, Shown); fn after() {}",
        &["T", "Shown", "after"],
        &["Map", "A", "B"],
    ),
    (
        "enum variant with a body",
        "enum E { A, #[cfg(test)] V { a: u8 }, B } fn after() {}",
        &["E", "A", "B", "after"],
        &["V", "a"],
    ),
    (
        "last enum variant with a tuple body and no comma",
        "enum E { A, #[cfg(test)] V(Hidden) } fn after() {}",
        &["E", "A", "after"],
        &["V", "Hidden"],
    ),
    (
        "enum variant with a discriminant",
        "enum E { A = 1, #[cfg(test)] B = 2, C } fn after() {}",
        &["E", "A", "C", "after"],
        &["B"],
    ),
    (
        "let statement followed by production statements",
        "fn f() { #[cfg(test)] let hidden = make(); let shown = real(); other(); }",
        &["f", "shown", "real", "other"],
        &["hidden", "make"],
    ),
    (
        "let statement with a struct literal and a type with commas",
        "fn f() { #[cfg(test)] let h: Map<A, B> = Foo { a: 1 }; shown(); }",
        &["f", "shown"],
        &["h", "Map", "A", "B", "Foo", "a"],
    ),
    (
        "expression statement",
        "fn f() { #[cfg(test)] helper(1, 2); real(); }",
        &["f", "real"],
        &["helper"],
    ),
    (
        "block statement",
        "fn f() { #[cfg(test)] { hidden(); } shown(); }",
        &["f", "shown"],
        &["hidden"],
    ),
    (
        "tail expression before the closing brace",
        "fn f() { shown(); #[cfg(test)] hidden() } fn after() {}",
        &["f", "shown", "after"],
        &["hidden"],
    ),
    (
        "if expression statement with else",
        "fn f() { #[cfg(test)] if a { hidden() } else { other() } shown(); }",
        &["f", "shown"],
        &["a", "hidden", "other"],
    ),
    (
        "macro statement with braces inside a function",
        "fn f() { #[cfg(test)] hidden! { a, b } shown(); }",
        &["f", "shown"],
        &["hidden", "a", "b"],
    ),
    (
        "match arm with an expression",
        "fn f(x: P) { match x { A => one(), #[cfg(test)] B => hidden(), _ => two() } }",
        &["f", "x", "P", "A", "one", "two", "_"],
        &["B", "hidden"],
    ),
    (
        "match arm with a struct pattern and a block",
        "fn f(x: P) { match x { #[cfg(test)] Q { a } => { hidden(a); } _ => shown() } }",
        &["f", "x", "P", "shown", "_"],
        &["Q", "a", "hidden"],
    ),
    (
        "last match arm without a comma",
        "fn f(x: P) { match x { A => one(), #[cfg(test)] B => hidden() } after(); }",
        &["f", "x", "P", "A", "one", "after"],
        &["B", "hidden"],
    ),
    (
        "fn parameter",
        "fn f(a: u8, #[cfg(test)] hidden: Hidden, b: u8) { shown() }",
        &["f", "a", "b", "shown"],
        &["hidden", "Hidden"],
    ),
    (
        "last fn parameter",
        "fn f(a: u8, #[cfg(test)] hidden: Map<A, B>) { shown() }",
        &["f", "a", "shown"],
        &["hidden", "Map", "A", "B"],
    ),
    (
        "generic parameter",
        "struct G<A, #[cfg(test)] Hidden, B>(A, B); fn after() {}",
        &["G", "A", "B", "after"],
        &["Hidden"],
    ),
    (
        "last generic parameter",
        "fn f<A, #[cfg(test)] Hidden>() { shown() }",
        &["f", "A", "shown"],
        &["Hidden"],
    ),
    (
        "array element",
        "fn f() { let v = [a, #[cfg(test)] hidden, b]; }",
        &["f", "v", "a", "b"],
        &["hidden"],
    ),
    (
        "call argument",
        "fn f() { call(a, #[cfg(test)] hidden(1, 2), b); }",
        &["f", "call", "a", "b"],
        &["hidden"],
    ),
    (
        "struct literal field",
        "fn f() { S { a: 1, #[cfg(test)] hidden: Map::new(), b: 3 }; }",
        &["f", "S", "a", "b"],
        &["hidden", "Map", "new"],
    ),
    (
        "use declaration",
        "#[cfg(test)] use hidden::{a, b}; fn after() {}",
        &["after"],
        &["hidden", "a", "b"],
    ),
    (
        "impl block followed by a function",
        "#[cfg(test)] impl Hidden { fn m() {} } fn after() {}",
        &["after"],
        &["Hidden", "m"],
    ),
    (
        "const item",
        "#[cfg(test)] const C: u8 = 1; fn after() {}",
        &["after"],
        &["C"],
    ),
    (
        "static item with a struct literal",
        "#[cfg(test)] static S: Foo = Foo { x: 1 }; fn after() {}",
        &["after"],
        &["S", "Foo", "x"],
    ),
    (
        "function with generics and a where clause",
        "#[cfg(test)] fn hidden<A, B>() where A: X, B: Y { inner() } fn after() {}",
        &["after"],
        &["hidden", "A", "B", "X", "Y", "inner"],
    ),
    (
        "macro invocation with braces",
        "#[cfg(test)] hidden_macro! { a, b } fn after() {}",
        &["after"],
        &["hidden_macro", "a", "b"],
    ),
    (
        "macro invocation with parentheses",
        "#[cfg(test)] hidden_macro!(a, b); fn after() {}",
        &["after"],
        &["hidden_macro", "a", "b"],
    ),
    (
        "macro invocation with brackets",
        "#[cfg(test)] hidden_macro![a, b]; fn after() {}",
        &["after"],
        &["hidden_macro", "a", "b"],
    ),
    (
        "macro_rules definition",
        "#[cfg(test)] macro_rules! hidden { ($a:expr, $b:expr) => { $a + $b }; } fn after() {}",
        &["after"],
        &["hidden", "a", "expr", "b"],
    ),
    (
        "impl members",
        "impl X { #[cfg(test)] fn hidden(&self) {} #[cfg(test)] const C: u8 = 1; #[cfg(test)] type A = Foo; fn shown(&self) {} }",
        &["X", "shown"],
        &["hidden", "C", "A", "Foo"],
    ),
    (
        "trait members",
        "trait T { #[cfg(test)] fn hidden(&self); fn shown(&self); }",
        &["T", "shown"],
        &["hidden"],
    ),
    (
        "extern block members",
        "extern \"C\" { #[cfg(test)] fn hidden(); fn shown(); }",
        &["shown"],
        &["hidden"],
    ),
    (
        "module member",
        "mod m { #[cfg(test)] fn hidden() {} fn shown() {} } fn after() {}",
        &["m", "shown", "after"],
        &["hidden"],
    ),
    (
        "stacked attributes",
        "struct S { #[cfg(test)] #[allow(dead_code)] hidden: u8, shown: u8 } fn after() {}",
        &["S", "shown", "after"],
        &["allow", "dead_code", "hidden"],
    ),
    (
        "cfg(all(test, ..)) is a test region",
        "struct S { #[cfg(all(test, unix))] hidden: u8, shown: u8 } fn after() {}",
        &["S", "shown", "after"],
        &["unix", "hidden"],
    ),
    (
        "cfg_attr(test, ..) is not a test region",
        "struct S { #[cfg_attr(test, derive(Debug))] shown: u8, other: u8 } fn after() {}",
        &["S", "cfg_attr", "derive", "Debug", "shown", "other", "after"],
        &[],
    ),
    (
        "cfg(not(test)) is production",
        "struct S { #[cfg(not(test))] shown: u8 } fn after() {}",
        &["S", "shown", "after"],
        &[],
    ),
    (
        "test function",
        "#[test] fn hidden() { x(); } fn after() {}",
        &["after"],
        &["hidden", "x"],
    ),
    (
        "async gpui test function",
        "#[gpui::test] async fn hidden(cx: &mut Cx) {} fn after() {}",
        &["after"],
        &["gpui", "hidden", "cx", "Cx"],
    ),
    (
        "attribute as the last thing in a file",
        "fn before() {} #[cfg(test)]",
        &["before"],
        &[],
    ),
    (
        "attribute on an item without a terminator (unknown extent: production)",
        "fn before() {} #[cfg(test)] fn partial()",
        &["before", "partial"],
        &[],
    ),
    (
        "unbalanced body (unknown extent: production)",
        "#[cfg(test)] fn open() { fn inner() {} ",
        &["open", "inner"],
        &[],
    ),
    (
        "mis-nested brackets (unknown extent: production)",
        "fn f() { #[cfg(test)] x( ]; } fn after() { std::thread::spawn(|| {}); }",
        &["f", "x", "after", "std", "thread", "spawn"],
        &[],
    ),
    (
        "comparison is not mistaken for generics",
        "fn f() { call(#[cfg(test)] a < b, c > (d)); } fn after() {}",
        &["f", "call", "c", "d", "after"],
        &["a", "b"],
    ),
];

#[test]
fn region_end_for_every_attribute_position() {
    let mut failures = Vec::new();
    for (position, src, production, test_only) in POSITIONS {
        let (prod, test) = split_idents(src);
        if prod != sorted(production) || test != sorted(test_only) {
            failures.push(format!(
                "{position}: {src}\n  production {prod:?} (want {production:?})\n  test-only  {test:?} (want {test_only:?})"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn path_redirected_test_module_is_not_recorded() {
    let s =
        scan("#[cfg(test)]\n#[path = \"elsewhere.rs\"]\nmod tests;\n#[cfg(test)]\nmod real_tests;");
    assert_eq!(s.test_mod_decls, ["real_tests"]);
}

#[test]
fn inner_cfg_test_only_counts_before_the_first_item() {
    // After an item an inner attribute is not valid Rust: do not trust it.
    let s = scan("fn a() {}\n#![cfg(test)]\nfn b() {}");
    assert!(!s.whole_file_test);
    let src = "mod m {\n    fn a() {}\n    #![cfg(test)]\n    fn b() {}\n}\nfn after() {}";
    assert!(prod_idents(src).contains(&"b".to_string()));
    // An unbalanced module with a valid leading attribute is still fully classified.
    assert!(scan("#![cfg(test)]\nfn a() {").whole_file_test);
}

#[test]
fn unbalanced_input_never_hides_the_rest_of_the_file() {
    for src in [
        "#[cfg(test)] mod t { fn a() { std::thread::spawn(|| {}); }",
        "#[cfg(test)] fn a( { } fn b() { std::thread::spawn(|| {}); }",
        "fn z() { #![cfg(test)] fn y() { std::thread::spawn(|| {}); }",
    ] {
        let s = scan(src);
        let spawn = (0..s.tokens.len()).find(|i| s.text(src, *i) == "spawn");
        assert!(spawn.is_some_and(|i| !s.in_test[i]), "{src}");
    }
}

// ---- property: whatever precedes, a complete marker at file level is production -------

mod property {
    use super::*;
    use proptest::prelude::*;

    const MARKER: &str = "fn zz_marker() { std::thread::spawn(|| {}); }";

    /// Complete, balanced fragments: test-only constructs in every position (wrapped so the
    /// attribute's construct is complete), plain production items, and nested containers.
    fn leaf() -> impl Strategy<Value = String> {
        let mut all: Vec<String> = POSITIONS
            .iter()
            .map(|(_, src, _, _)| src.to_string())
            // The deliberately broken table cases are not complete fragments.
            .filter(|src| !src.ends_with("#[cfg(test)]") && !src.ends_with("partial()"))
            .filter(|src| !src.contains("open() {") && !src.contains("x( ]"))
            .collect();
        // Also without the trailing `fn after() {}`, so the marker directly follows the
        // construct and nothing absorbs a region that runs on.
        let bare: Vec<String> = all
            .iter()
            .filter_map(|src| src.strip_suffix(" fn after() {}"))
            .map(str::to_string)
            .collect();
        all.extend(bare);
        for extra in [
            "fn plain() { helper(1, 2); }",
            "#[cfg(test)] mod tests { fn t() { x(); } }",
            "#[cfg(test)] mod tests;",
            "#[test] fn t() {}",
            "#[cfg(test)] #[derive(Debug)] struct S { #[cfg(test)] a: Map<A, B> }",
            "enum E { #[cfg(test)] A { x: u8 }, #[cfg(test)] B(u8), C }",
            "struct T(#[cfg(test)] u8, #[cfg(test)] Map<A, B>);",
            "impl X { #[cfg(test)] fn a() {} #[cfg(test)] fn b() {} fn c() {} }",
            "#[cfg(test)] use a::{b, c};",
            "#[cfg(test)] const C: [u8; 3] = [1, 2, 3];",
            "fn g() { #[cfg(test)] let a = 1; #[cfg(test)] let b = 2; real(); }",
        ] {
            all.push(extra.to_string());
        }
        proptest::sample::select(all)
    }

    fn fragment() -> impl Strategy<Value = String> {
        leaf().prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4)
                    .prop_map(|v| format!("mod m {{ {} }}", v.join(" "))),
                proptest::collection::vec(inner, 0..4)
                    .prop_map(|v| format!("#[cfg(test)] mod t {{ {} }}", v.join(" "))),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn marker_after_a_balanced_prefix_is_never_test_only(
            prefix in proptest::collection::vec(fragment(), 0..6),
            sep in prop_oneof![Just(" "), Just("\n"), Just("\n\n")],
        ) {
            let src = format!("{}{sep}{MARKER}", prefix.join(sep));
            let s = scan(&src);
            prop_assert!(!s.whole_file_test);
            let from = src.len() - MARKER.len();
            let marker_tokens: Vec<usize> =
                (0..s.tokens.len()).filter(|i| s.tokens[*i].start >= from).collect();
            prop_assert!(marker_tokens.len() > 10);
            for i in marker_tokens {
                prop_assert!(!s.in_test[i], "token {} of {:?}", s.text(&src, i), src);
            }
        }
    }
}
