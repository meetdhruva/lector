//! Content identity and the memoization seam of the review functions.
//!
//! The review functions parse both sides of a diff and compute line diffs. Both are pure
//! functions of the content, so a caller that keeps a cache between calls (the app's
//! content-addressed `lector_app::content_cache::ContentCache`) can answer them from memory.
//! This crate keeps no such cache itself (ADR 0001): every function that would use one takes a
//! `&dyn DerivedCache`. [`NoCache`] computes on every call, for tools, tests and benchmarks
//! that do not want a cache.

use std::sync::Arc;

use lector_diff::{DiffOptions, LineDiff};
use lector_git::Oid;
use lector_syntax::{Language, Syntax, SyntaxError};
use lector_text::Text;

/// Identity of a piece of content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentId {
    /// A git blob.
    Blob(Oid),
    /// A 128-bit hash of the bytes (see [`ContentId::of_bytes`]).
    Hash(u128),
}

impl ContentId {
    /// Hashes `bytes` (fast, non-cryptographic, 128 bits, stable across runs).
    pub fn of_bytes(bytes: &[u8]) -> ContentId {
        const K1: u64 = 0x9E37_79B9_7F4A_7C15;
        const K2: u64 = 0xC2B2_AE3D_27D4_EB4F;
        fn mix(mut h: u64) -> u64 {
            h ^= h >> 33;
            h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
            h ^= h >> 33;
            h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
            h ^ (h >> 33)
        }
        let (mut a, mut b) = (K1 ^ bytes.len() as u64, K2);
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let v = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
            a = (a ^ v).wrapping_mul(K1).rotate_left(29);
            b = (b.wrapping_add(v)).wrapping_mul(K2) ^ (b >> 31);
        }
        let mut tail = [0u8; 8];
        let rest = chunks.remainder();
        tail[..rest.len()].copy_from_slice(rest);
        let v = u64::from_le_bytes(tail);
        a = (a ^ v).wrapping_mul(K1).rotate_left(29);
        b = (b.wrapping_add(v)).wrapping_mul(K2) ^ (b >> 31);
        ContentId::Hash(
            (mix(a ^ b.rotate_left(17)) as u128) << 64 | mix(b ^ a.rotate_left(41)) as u128,
        )
    }

    /// Hashes text (its UTF-8 bytes).
    pub fn of_text(text: &Text) -> ContentId {
        match text.as_rope() {
            Some(rope) => {
                // Hash chunk by chunk without materializing the text.
                let mut buf = Vec::with_capacity(rope.len());
                for c in rope.chunks() {
                    buf.extend_from_slice(c.as_bytes());
                }
                ContentId::of_bytes(&buf)
            }
            None => ContentId::of_bytes(text.text_for_range(0..text.len()).as_bytes()),
        }
    }
}

impl From<Oid> for ContentId {
    fn from(oid: Oid) -> Self {
        ContentId::Blob(oid)
    }
}

/// Derived data of content, possibly memoized by the caller between calls.
///
/// Implementations must return exactly what the uncached computation returns: the review
/// functions give identical results with any implementation (see [`NoCache`]).
pub trait DerivedCache: Send + Sync {
    /// `Syntax::parse(lang, text)` for the content `id`.
    fn syntax(
        &self,
        id: ContentId,
        lang: &Arc<Language>,
        text: &Text,
    ) -> Result<Syntax, SyntaxError>;

    /// The line diff between the contents `old` and `new` under `opts`; `compute` produces it.
    fn line_diff(
        &self,
        old: ContentId,
        new: ContentId,
        opts: &DiffOptions,
        compute: &dyn Fn() -> LineDiff,
    ) -> LineDiff;
}

/// A [`DerivedCache`] that computes everything on every call.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCache;

impl DerivedCache for NoCache {
    fn syntax(
        &self,
        _id: ContentId,
        lang: &Arc<Language>,
        text: &Text,
    ) -> Result<Syntax, SyntaxError> {
        Syntax::parse(lang, text)
    }

    fn line_diff(
        &self,
        _old: ContentId,
        _new: ContentId,
        _opts: &DiffOptions,
        compute: &dyn Fn() -> LineDiff,
    ) -> LineDiff {
        compute()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_id_is_stable_and_discriminating() {
        let a = ContentId::of_bytes(b"hello world");
        assert_eq!(a, ContentId::of_bytes(b"hello world"));
        assert_ne!(a, ContentId::of_bytes(b"hello worle"));
        assert_ne!(ContentId::of_bytes(b""), ContentId::of_bytes(b"\0"));
        assert_ne!(
            ContentId::of_bytes(b"abcdefgh"),
            ContentId::of_bytes(b"abcdefgh\0")
        );
        assert_eq!(ContentId::of_text(&Text::from("hello world")), a);
    }
}
