//! Item structure the rules share: `impl` blocks and `fn` items, found from tokens alone.
//!
//! Not a parser: an `impl` is the keyword, optional generics, an optional `Trait for`, and the
//! last identifier of the self type's path; a `fn` is the keyword, a name, optional generics, a
//! parameter list, an optional `-> type` and a body or `;`. Anything stranger (macro-generated
//! items, `fn` inside macro bodies that do not look like items) is simply not found, which the
//! rules document as a limit.

use crate::lexer::TokKind;
use crate::source::SourceFile;

/// An `impl` block.
#[derive(Debug, Clone)]
pub struct ImplBlock {
    /// Last identifier of the self type's path (`Repo` for `impl<'a> lector_git::Repo<'a>`).
    pub ty: String,
    /// `impl Trait for Type` (as opposed to an inherent impl).
    pub trait_impl: bool,
    /// Index of the first token inside the braces.
    pub open: usize,
    /// Index of the closing brace.
    pub close: usize,
}

/// A `fn` item.
#[derive(Debug, Clone)]
pub struct FnItem {
    /// Function name.
    pub name: String,
    /// Token index of the name.
    pub name_idx: usize,
    /// Plain `pub` (not `pub(crate)` and friends).
    pub is_pub: bool,
    /// The first parameter is `self`, `&self`, `&mut self` or `mut self`.
    pub takes_self: bool,
    /// Number of parameters, `self` excluded.
    pub params: usize,
    /// Token range of the return type (empty when there is none).
    pub ret: (usize, usize),
    /// Token range inside the body braces, when the function has one.
    pub body: Option<(usize, usize)>,
    /// Innermost enclosing `impl`, if any.
    pub impl_ty: Option<String>,
    /// The enclosing `impl` is a trait impl.
    pub trait_impl: bool,
}

fn is_ident(f: &SourceFile, i: usize) -> bool {
    f.tokens().get(i).is_some_and(|t| t.kind == TokKind::Ident)
}

/// Index just past the angle-bracket group opening at `j` (`j` when there is none).
pub fn skip_angles(f: &SourceFile, j: usize) -> usize {
    if f.tok(j) != "<" {
        return j;
    }
    let n = f.tokens().len();
    let mut depth = 0usize;
    let mut k = j;
    while k < n {
        match f.tok(k) {
            "<" => depth += 1,
            ">" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k + 1;
                }
            }
            "{" | ";" => return j,
            _ => {}
        }
        k += 1;
    }
    j
}

/// Last identifier of the type path starting at `j` (skipping `&`, `mut`, `dyn`, lifetimes).
fn type_name_at(f: &SourceFile, mut j: usize) -> String {
    while matches!(f.tok(j), "&" | "mut" | "dyn" | "*" | "const")
        || f.tokens()
            .get(j)
            .is_some_and(|t| t.kind == TokKind::Lifetime)
    {
        j += 1;
    }
    if f.tok(j) == "::" {
        j += 1;
    }
    let mut name = String::new();
    while is_ident(f, j) {
        name = f.tok(j).to_string();
        if f.tok(j + 1) == "::" {
            j += 2;
        } else {
            break;
        }
    }
    name
}

/// Every `impl` block with a brace body.
pub fn impl_blocks(f: &SourceFile) -> Vec<ImplBlock> {
    let n = f.tokens().len();
    let mut out = Vec::new();
    for i in 0..n {
        if f.tok(i) != "impl" || !is_ident(f, i) || (i > 0 && f.tok(i - 1) == "::") {
            continue;
        }
        let mut j = skip_angles(f, i + 1);
        if f.tok(j) == "!" {
            j += 1;
        }
        let first = j;
        let mut k = j;
        let mut for_at = None;
        while k < n && f.tok(k) != "{" && f.tok(k) != ";" && f.tok(k) != "where" {
            if f.tok(k) == "for" {
                for_at = Some(k);
            }
            k += 1;
        }
        let mut brace = k;
        while brace < n && f.tok(brace) != "{" && f.tok(brace) != ";" {
            brace += 1;
        }
        if f.tok(brace) != "{" {
            continue;
        }
        let ty = type_name_at(f, for_at.map_or(first, |p| p + 1));
        out.push(ImplBlock {
            ty,
            trait_impl: for_at.is_some(),
            open: brace + 1,
            close: f.close_of(brace),
        });
    }
    out
}

/// Number of parameters of the list opening at `open`, and whether the first is `self`.
pub fn param_list(f: &SourceFile, open: usize) -> (bool, usize) {
    let close = f.close_of(open);
    let (mut depth, mut angle) = (0usize, 0usize);
    let mut segments = 0usize;
    let mut at_start = true;
    for k in open + 1..close {
        match f.tok(k) {
            "," if depth == 0 && angle == 0 => {
                at_start = true;
                continue;
            }
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            "<" if depth == 0 => angle += 1,
            ">" if depth == 0 => angle = angle.saturating_sub(1),
            _ => {}
        }
        if at_start {
            segments += 1;
            at_start = false;
        }
    }
    let mut j = open + 1;
    if f.tok(j) == "&" {
        j += 1;
        if f.tokens()
            .get(j)
            .is_some_and(|t| t.kind == TokKind::Lifetime)
        {
            j += 1;
        }
    }
    if f.tok(j) == "mut" {
        j += 1;
    }
    let takes_self = f.tok(j) == "self";
    (takes_self, segments - usize::from(takes_self))
}

fn is_plain_pub(f: &SourceFile, fn_idx: usize) -> bool {
    let mut j = fn_idx;
    while j > 0 {
        j -= 1;
        let t = f.tok(j);
        let is_abi = f.tokens().get(j).is_some_and(|t| t.kind == TokKind::Str);
        if matches!(t, "const" | "async" | "unsafe" | "extern" | "default") || is_abi {
            continue;
        }
        return t == "pub";
    }
    false
}

/// Every `fn` item with a name (function-pointer types `fn(..)` are skipped).
pub fn fn_items(f: &SourceFile) -> Vec<FnItem> {
    let n = f.tokens().len();
    let impls = impl_blocks(f);
    let mut out = Vec::new();
    for i in 0..n {
        if f.tok(i) != "fn" || !is_ident(f, i) || !is_ident(f, i + 1) {
            continue;
        }
        let name_idx = i + 1;
        let open = skip_angles(f, name_idx + 1);
        if f.tok(open) != "(" {
            continue;
        }
        let close = f.close_of(open);
        let (takes_self, params) = param_list(f, open);
        let mut k = close + 1;
        let mut ret = (close + 1, close + 1);
        if f.tok(k) == "->" {
            let start = k + 1;
            let (mut depth, mut angle) = (0usize, 0usize);
            k = start;
            while k < n {
                match f.tok(k) {
                    "(" | "[" => depth += 1,
                    ")" | "]" => depth = depth.saturating_sub(1),
                    "<" => angle += 1,
                    ">" => angle = angle.saturating_sub(1),
                    "{" | ";" if depth == 0 && angle == 0 => break,
                    "where" if depth == 0 => break,
                    _ => {}
                }
                k += 1;
            }
            ret = (start, k);
        }
        // Skip a `where` clause to the body or `;`.
        let mut depth = 0usize;
        while k < n {
            match f.tok(k) {
                "(" | "[" => depth += 1,
                ")" | "]" => depth = depth.saturating_sub(1),
                "{" | ";" if depth == 0 => break,
                _ => {}
            }
            k += 1;
        }
        let body = (f.tok(k) == "{").then(|| (k + 1, f.close_of(k)));
        let enclosing = impls
            .iter()
            .filter(|b| b.open <= i && i < b.close)
            .max_by_key(|b| b.open);
        out.push(FnItem {
            name: f.tok(name_idx).to_string(),
            name_idx,
            is_pub: is_plain_pub(f, i),
            takes_self,
            params,
            ret,
            body,
            impl_ty: enclosing.map(|b| b.ty.clone()),
            trait_impl: enclosing.is_some_and(|b| b.trait_impl),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    #[test]
    fn impl_blocks_and_their_types() {
        let f = file(
            "a.rs",
            "impl Repo { fn a(&self) {} }\nimpl<'a, T: X> lector_git::ShadowRepo<'a> where T: Y { fn b() {} }\nimpl std::fmt::Debug for Repo { fn fmt(&self) {} }\nimpl<T> From<T> for &Foo {}\nimpl Trait for Vec<Repo> { }",
        );
        let blocks: Vec<(String, bool)> = impl_blocks(&f)
            .into_iter()
            .map(|b| (b.ty, b.trait_impl))
            .collect();
        assert_eq!(
            blocks,
            [
                ("Repo".into(), false),
                ("ShadowRepo".into(), false),
                ("Repo".into(), true),
                ("Foo".into(), true),
                ("Vec".into(), true),
            ]
        );
    }

    #[test]
    fn fn_items_visibility_params_return_and_context() {
        let src = "impl Repo {\n pub fn a(&self, x: &[u8], m: HashMap<K, V>) -> Result<Oid> { 1 }\n pub(crate) fn b(&self) {}\n pub const fn c() -> u8 { 0 }\n pub async fn d(mut self) {}\n fn e(&mut self) -> &Repo { x }\n}\nimpl Drop for Repo { fn drop(&mut self) {} }\npub fn free<T>(a: T) -> impl Fn(u8) -> u8 where T: Copy { |x| x }\nfn ptr(f: fn(u8) -> u8) {}\ntrait T { fn decl(&self) -> u8; }";
        let f = file("a.rs", src);
        let items = fn_items(&f);
        let desc: Vec<String> = items
            .iter()
            .map(|i| {
                format!(
                    "{}:{}:{}:{}:{}:{}",
                    i.name,
                    i.is_pub,
                    i.takes_self,
                    i.params,
                    i.impl_ty.clone().unwrap_or_default(),
                    i.trait_impl
                )
            })
            .collect();
        assert_eq!(
            desc,
            [
                "a:true:true:2:Repo:false",
                "b:false:true:0:Repo:false",
                "c:true:false:0:Repo:false",
                "d:true:true:0:Repo:false",
                "e:false:true:0:Repo:false",
                "drop:false:true:0:Repo:true",
                "free:true:false:1::false",
                "ptr:false:false:1::false",
                "decl:false:true:0::false",
            ]
        );
        let a = &items[0];
        let ret: Vec<&str> = (a.ret.0..a.ret.1).map(|i| f.tok(i)).collect();
        assert_eq!(ret.join(""), "Result<Oid>");
        assert!(a.body.is_some());
        let e = &items[4];
        assert!((e.ret.0..e.ret.1).any(|i| f.tok(i) == "Repo"));
        assert!(items[8].body.is_none());
    }
}
