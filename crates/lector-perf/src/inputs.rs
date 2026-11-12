//! Seeded generators for gate inputs. Everything is a pure function of its arguments: the same
//! call gives the same bytes on every machine, so instruction counts depend on the code only.

/// A small deterministic PRNG (splitmix64).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator for `seed`.
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound` of 0 gives 0).
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }

    /// One element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len()))
    }
}

const IDENTS: &[&str] = &[
    "value", "buffer", "cursor", "offset", "length", "parser", "token", "index", "result",
    "config", "state", "handle", "layout", "region", "anchor", "window",
];
const CALLS: &[&str] = &[
    "compute", "resolve", "advance", "collect", "update", "measure", "render", "lookup",
];

/// `lines` lines of Rust-looking source (about 45 bytes each): functions, lets, comments and
/// blank lines, with some non-ASCII text so UTF-8 paths are exercised.
pub fn rust_like_source(lines: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity(lines * 48);
    let mut i = 0;
    while i < lines {
        let id = IDENTS[rng.below(IDENTS.len())];
        let call = CALLS[rng.below(CALLS.len())];
        let n = rng.below(1000);
        match i % 8 {
            0 => out.push_str(&format!("pub fn {call}_{id}_{i}(x: u32) -> u32 {{\n")),
            1 => out.push_str(&format!("    let {id}_{n} = {call}(x, {n});\n")),
            2 => out.push_str(&format!("    // note {i}: {id} wörld {n}\n")),
            3 => out.push_str(&format!(
                "    if {id}_{n} > {i} {{ return {call}({id}_{n}); }}\n"
            )),
            4 => out.push_str(&format!("    let s = \"{id} {n}\".to_string();\n")),
            5 => out.push('\n'),
            6 => out.push_str(&format!("    {id}_{n}.wrapping_add({n})\n")),
            _ => out.push_str("}\n"),
        }
        i += 1;
    }
    out
}

/// Syntactically valid Rust of about `lines` lines: structs, impls, matches, closures, strings,
/// comments and attributes, in 22-line blocks that differ in names and numbers.
pub fn rust_module(lines: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity(lines * 34);
    let mut n = 0;
    while out.lines().count() < lines {
        let id = IDENTS[rng.below(IDENTS.len())];
        let call = CALLS[rng.below(CALLS.len())];
        let k = rng.below(1000);
        out.push_str(&format!(
            "/// Documentation for item {n} ({id}).
#[derive(Debug, Clone, PartialEq)]
pub struct Item{n} {{
    pub id: u32,
    name: String,
}}

impl Item{n} {{
    pub fn new(id: u32) -> Self {{
        let name = format!(\"{id}-{{}}\", id + {k});
        Self {{ id, name }}
    }}

    pub fn {call}(&self, other: &Item{n}) -> i64 {{
        // compare by id, then by name ({k})
        match self.id.cmp(&other.id) {{
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal if self.name == other.name => 0,
            _ => [1, 2, 3].iter().map(|v| v * {k}).sum::<i64>(),
        }}
    }}
}}

"
        ));
        n += 1;
    }
    out
}

/// A copy of `text` with about `percent` percent of its lines modified (a token appended or
/// replaced), plus an occasional inserted and deleted line, like an edited file.
pub fn edited_copy(text: &str, percent: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    for line in text.split_inclusive('\n') {
        let roll = rng.below(1000);
        let body = line.strip_suffix('\n').unwrap_or(line);
        if roll < percent * 10 {
            let id = IDENTS[rng.below(IDENTS.len())];
            match rng.below(3) {
                0 => out.push_str(&format!("{body} // {id}\n")),
                1 => out.push_str(&body.replace("let", "let mut")),
                _ => {
                    let cut = body
                        .char_indices()
                        .nth(body.chars().count() / 2)
                        .map_or(0, |(i, _)| i);
                    out.push_str(&format!(
                        "{}{id}_changed({})\n",
                        &body[..cut],
                        rng.below(99)
                    ));
                }
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
        } else if roll < percent * 10 + 8 {
            // Deleted line.
        } else if roll < percent * 10 + 16 {
            out.push_str(line);
            out.push_str(&format!(
                "    let inserted_{} = {};\n",
                rng.below(9999),
                roll
            ));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// `count` synthetic project paths such as `crates/lector-core/src/parser/module_17_4242.rs`
/// (unique, a few hundred distinct directories, mixed extensions).
pub fn project_paths(count: usize, seed: u64) -> Vec<String> {
    const TOP: &[&str] = &[
        "crates", "src", "tests", "docs", "vendor", "packages", "apps", "lib",
    ];
    const MID: &[&str] = &[
        "core", "editor", "parser", "render", "network", "storage", "search", "theme", "git", "ui",
    ];
    const LEAF: &[&str] = &[
        "src", "bin", "util", "model", "view", "tests", "benches", "fixtures",
    ];
    const EXT: &[&str] = &["rs", "rs", "rs", "toml", "md", "json", "ts", "py"];
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|i| {
            let ext = EXT[rng.below(EXT.len())];
            format!(
                "{}/{}-{}/{}/module_{}_{}.{}",
                TOP[rng.below(TOP.len())],
                MID[rng.below(MID.len())],
                rng.below(40),
                LEAF[rng.below(LEAF.len())],
                rng.below(1000),
                i,
                ext
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_are_deterministic() {
        assert_eq!(rust_like_source(300, 1), rust_like_source(300, 1));
        assert_ne!(rust_like_source(300, 1), rust_like_source(300, 2));
        let a = rust_like_source(500, 3);
        assert_eq!(edited_copy(&a, 5, 9), edited_copy(&a, 5, 9));
        assert_ne!(edited_copy(&a, 5, 9), a);
        assert_eq!(project_paths(100, 4), project_paths(100, 4));
    }

    #[test]
    fn rust_module_is_sized_and_deterministic() {
        let a = rust_module(2000, 8);
        assert_eq!(a, rust_module(2000, 8));
        let n = a.lines().count();
        assert!((2000..2030).contains(&n), "{n}");
    }

    #[test]
    fn sources_have_the_requested_shape() {
        let src = rust_like_source(2000, 7);
        assert_eq!(src.lines().count(), 2000);
        assert!(src.ends_with('\n'));
        let paths = project_paths(1000, 5);
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), 1000);
    }

    #[test]
    fn rng_helpers_stay_in_range() {
        let mut rng = Rng::new(0);
        for _ in 0..1000 {
            assert!(rng.below(7) < 7);
        }
        assert_eq!(rng.below(0), 0);
        assert!(rng.pick::<u8>(&[]).is_none());
        assert!(rng.pick(&[1, 2, 3]).is_some());
    }
}
