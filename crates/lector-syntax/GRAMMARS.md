# Grammars and queries

`lector-syntax` links tree-sitter grammars as Rust crates (all compatible with the
`tree-sitter-language` ABI crate, so they work with `tree-sitter` 0.27). Every
grammar below is **MIT** licensed (verified from each crate's `Cargo.toml` and
LICENSE file); the runtime (`tree-sitter`, `tree-sitter-language`) is MIT too.

| Language | Crate | Version | License | Upstream | Highlights | Tags | Outline / injections / extras |
|---|---|---|---|---|---|---|---|
| Rust | `tree-sitter-rust` | 0.24.2 | MIT | tree-sitter/tree-sitter-rust | crate + `queries/rust/highlights-extra.scm` (number/boolean overrides) | crate | own outline |
| TypeScript, TSX | `tree-sitter-typescript` | 0.23.2 | MIT | tree-sitter/tree-sitter-typescript | JS query + TS query (+ JSX query for TSX) | crate | own outline (shared) |
| JavaScript (+JSX) | `tree-sitter-javascript` | 0.25.0 | MIT | tree-sitter/tree-sitter-javascript | crate (+ JSX query) | crate | own outline |
| Python | `tree-sitter-python` | 0.25.0 | MIT | tree-sitter/tree-sitter-python | crate | crate | own outline |
| Go | `tree-sitter-go` | 0.25.0 | MIT | tree-sitter/tree-sitter-go | crate | crate | own outline |
| Java | `tree-sitter-java` | 0.23.5 | MIT | tree-sitter/tree-sitter-java | crate | crate | own outline |
| Kotlin | `tree-sitter-kotlin-ng` | 1.1.0 | MIT | tree-sitter-grammars/tree-sitter-kotlin | **own** `queries/kotlin/highlights.scm` (crate ships none) | **own** | own outline |
| Swift | `tree-sitter-swift` | 0.7.4 | MIT | alex-pinkus/tree-sitter-swift | crate | crate | own outline |
| C | `tree-sitter-c` | 0.24.2 | MIT | tree-sitter/tree-sitter-c | crate | crate | own outline |
| C++ | `tree-sitter-cpp` | 0.23.4 | MIT | tree-sitter/tree-sitter-cpp | C query + C++ query | crate | own outline |
| C# | `tree-sitter-c-sharp` | 0.23.5 | MIT | tree-sitter/tree-sitter-c-sharp | crate | crate | own outline |
| Ruby | `tree-sitter-ruby` | 0.23.1 | MIT | tree-sitter/tree-sitter-ruby | crate | crate | own outline |
| PHP | `tree-sitter-php` | 0.25.1 | MIT | tree-sitter/tree-sitter-php | crate | crate | own outline |
| Bash | `tree-sitter-bash` | 0.25.1 | MIT | tree-sitter/tree-sitter-bash | crate | **own** | own outline |
| HTML | `tree-sitter-html` | 0.23.2 | MIT | tree-sitter/tree-sitter-html | crate | – | own outline; injections: `<script>` → JavaScript, `<style>` → CSS |
| CSS | `tree-sitter-css` | 0.25.0 | MIT | tree-sitter/tree-sitter-css | crate | **own** | own outline |
| JSON, JSONC | `tree-sitter-json` | 0.24.8 | MIT | tree-sitter/tree-sitter-json | crate | – | own outline (JSONC = same grammar, comments are valid) |
| YAML | `tree-sitter-yaml` | 0.7.2 | MIT | tree-sitter-grammars/tree-sitter-yaml | crate | – | own outline |
| TOML | `tree-sitter-toml-ng` | 0.7.0 | MIT | tree-sitter-grammars/tree-sitter-toml | crate | – | own outline |
| Markdown | `tree-sitter-md` (block grammar) | 0.5.4 | MIT | tree-sitter-grammars/tree-sitter-markdown | crate | – | own outline (sections); injections: fenced code (by info string), front matter (YAML/TOML), HTML blocks, inline |
| Markdown Inline | `tree-sitter-md` (inline grammar) | 0.5.4 | MIT | same | crate | – | injection target only (hidden from `languages()`) |
| SQL | `tree-sitter-sequel` | 0.3.11 | MIT | DerekStride/tree-sitter-sql | crate | **own** | own outline |

## Where queries live

* `queries/<lang>/outline.scm` — written for Lector. Captures: `@item.<kind>` (the
  whole symbol node; `kind` is `module|class|struct|enum|interface|trait|function|method|field|variable|constant|type|macro|heading|impl|other`),
  `@name` (identifier node) and optional `@context` (extra words joined into the
  display name, e.g. `impl Display for Foo`).
* `queries/<lang>/highlights.scm` — only where the grammar crate ships none (Kotlin) or needs
  overrides (`rust/highlights-extra.scm`). All other highlight and tags queries come from the
  grammar crates' exported constants, so upstream improvements arrive with crate updates.
* `queries/<lang>/tags.scm` — written for Lector where the crate has none (Kotlin, Bash, CSS, SQL).
* `queries/<lang>/injections.scm` — Markdown, Markdown inline and HTML (written for Lector,
  derived from the upstream injection queries; all patterns use `injection.include-children`).

Our own query files are licensed MIT OR Apache-2.0 like the rest of Lector.

## Not included

No grammar was dropped: all requested languages (including SQL via `tree-sitter-sequel`) are
supported. `tree-sitter-kotlin` (fwcd) was not used because it targets an older tree-sitter ABI
crate; `tree-sitter-kotlin-ng` is the maintained grammar.
