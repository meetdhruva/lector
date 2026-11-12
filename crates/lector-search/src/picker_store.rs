//! Append-only, chunked storage for the fuzzy picker.
//!
//! Haystacks live in contiguous per-chunk byte arenas (one allocation per
//! 4096 items instead of one per item) next to dense side arrays that the
//! scan loop reads sequentially: end offsets, 128-bit character masks and
//! boosts. Items are appended by any thread through a mutex-protected builder;
//! full (or explicitly flushed) chunks are *sealed* into a lock-free table of
//! [`OnceLock`]s so that the matching thread and `Picker::matches` read them
//! without any lock and can hand out `&T` tied to the picker (no `unsafe`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

/// Items per full chunk.
pub(crate) const CHUNK_ITEMS: usize = 4096;
const SEG_CHUNKS: usize = 256;
const DIR_SEGS: usize = 512;
/// Total number of chunk slots (about 5e8 items when chunks are full).
const MAX_CHUNKS: usize = SEG_CHUNKS * DIR_SEGS;
/// Longest haystack that is stored (longer ones are cut at a char boundary).
const MAX_HAYSTACK: usize = 1 << 16;

/// Bit in [`Mask::any`] that is only set for non-ASCII haystacks (for which
/// every mask bit is set, so they never fail a mask test).
pub(crate) const NON_ASCII: u64 = 1 << 63;

/// Per-haystack prefilter data.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Mask {
    /// Characters (case folded, see [`char_bit`]) that occur anywhere.
    pub any: u64,
    /// Characters that occur at a word boundary or are not alphanumeric
    /// themselves: positions where nucleo can award a boundary bonus.
    pub boundary: u64,
}

/// Mask bit of an ASCII byte: letters share a bit across cases, digits and
/// `. _ - / space` have their own bits, the remaining ASCII bytes are hashed
/// into 22 shared bits (a superset test stays valid). Bit 63 is never used.
#[inline]
pub(crate) fn char_bit(b: u8) -> u64 {
    let bit = match b {
        b'a'..=b'z' => b - b'a',
        b'A'..=b'Z' => b - b'A',
        b'0'..=b'9' => 26 + (b - b'0'),
        b'.' => 36,
        b'_' => 37,
        b'-' => 38,
        b'/' => 39,
        b' ' => 40,
        other => 41 + other % 22,
    };
    1u64 << bit
}

/// Computes the [`Mask`] of a haystack. Non-ASCII haystacks get all bits set.
pub(crate) fn mask_of(hay: &[u8]) -> Mask {
    let mut any = 0u64;
    let mut boundary = 0u64;
    let mut prev_alnum = false;
    for &b in hay {
        if b >= 0x80 {
            return Mask {
                any: u64::MAX,
                boundary: u64::MAX,
            };
        }
        let bit = char_bit(b);
        any |= bit;
        let alnum = b.is_ascii_alphanumeric();
        if !alnum || !prev_alnum {
            boundary |= bit;
        }
        prev_alnum = alnum;
    }
    Mask { any, boundary }
}

/// An immutable block of items.
pub(crate) struct Chunk<T> {
    /// Global index of the first item.
    pub base: u32,
    bytes: Box<[u8]>,
    ends: Box<[u32]>,
    pub masks: Box<[Mask]>,
    pub boosts: Box<[u16]>,
    pub items: Box<[T]>,
}

impl<T> Chunk<T> {
    /// Number of items.
    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Haystack bytes (UTF-8) of item `j`.
    #[inline]
    pub fn hay(&self, j: usize) -> &[u8] {
        let end = self.ends[j] as usize;
        let start = if j == 0 { 0 } else { self.ends[j - 1] as usize };
        &self.bytes[start..end]
    }
}

struct Builder<T> {
    base: u32,
    bytes: Vec<u8>,
    ends: Vec<u32>,
    masks: Vec<Mask>,
    boosts: Vec<u16>,
    items: Vec<T>,
}

type Segment<T> = Box<[OnceLock<Chunk<T>>]>;

/// The chunked store. See the module docs.
pub(crate) struct Store<T> {
    dir: Box<[OnceLock<Segment<T>>]>,
    sealed_chunks: AtomicU32,
    sealed_items: AtomicU32,
    injected: AtomicU32,
    builder: Mutex<Builder<T>>,
}

impl<T> Store<T> {
    pub fn new() -> Self {
        Store {
            dir: (0..DIR_SEGS).map(|_| OnceLock::new()).collect(),
            sealed_chunks: AtomicU32::new(0),
            sealed_items: AtomicU32::new(0),
            injected: AtomicU32::new(0),
            builder: Mutex::new(Builder {
                base: 0,
                bytes: Vec::new(),
                ends: Vec::new(),
                masks: Vec::new(),
                boosts: Vec::new(),
                items: Vec::new(),
            }),
        }
    }

    /// Appends an item. Thread-safe.
    pub fn push(&self, item: T, haystack: &str, boost: u16) {
        let mut hay = haystack.as_bytes();
        if hay.len() > MAX_HAYSTACK {
            let mut cut = MAX_HAYSTACK;
            while cut > 0 && !haystack.is_char_boundary(cut) {
                cut -= 1;
            }
            hay = &hay[..cut];
        }
        let mask = mask_of(hay);
        let mut b = match self.builder.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if self.sealed_chunks.load(Ordering::Relaxed) as usize >= MAX_CHUNKS {
            return;
        }
        b.bytes.extend_from_slice(hay);
        let end = b.bytes.len() as u32;
        b.ends.push(end);
        b.masks.push(mask);
        b.boosts.push(boost);
        b.items.push(item);
        self.injected.fetch_add(1, Ordering::Release);
        if b.items.len() >= CHUNK_ITEMS || b.bytes.len() > (1 << 26) {
            self.seal(&mut b);
        }
    }

    /// Seals the partially filled chunk so the matching thread can see it.
    pub fn flush(&self) {
        let mut b = match self.builder.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        self.seal(&mut b);
    }

    fn seal(&self, b: &mut Builder<T>) {
        if b.items.is_empty() {
            return;
        }
        let no = self.sealed_chunks.load(Ordering::Relaxed) as usize;
        if no >= MAX_CHUNKS {
            return;
        }
        let len = b.items.len() as u32;
        let chunk = Chunk {
            base: b.base,
            bytes: std::mem::take(&mut b.bytes).into_boxed_slice(),
            ends: std::mem::take(&mut b.ends).into_boxed_slice(),
            masks: std::mem::take(&mut b.masks).into_boxed_slice(),
            boosts: std::mem::take(&mut b.boosts).into_boxed_slice(),
            items: std::mem::take(&mut b.items).into_boxed_slice(),
        };
        let seg = self.dir[no / SEG_CHUNKS]
            .get_or_init(|| (0..SEG_CHUNKS).map(|_| OnceLock::new()).collect());
        let _ = seg[no % SEG_CHUNKS].set(chunk);
        b.base += len;
        self.sealed_items.store(b.base, Ordering::Release);
        self.sealed_chunks.store(no as u32 + 1, Ordering::Release);
    }

    /// Items pushed so far (sealed or not).
    #[inline]
    pub fn injected(&self) -> u32 {
        self.injected.load(Ordering::Acquire)
    }

    /// Items visible to scans (all sealed chunks).
    #[inline]
    pub fn sealed_items(&self) -> u32 {
        self.sealed_items.load(Ordering::Acquire)
    }

    /// Items pushed but not yet sealed.
    #[inline]
    pub fn pending(&self) -> u32 {
        self.injected().saturating_sub(self.sealed_items())
    }

    /// Number of sealed chunks.
    pub fn chunk_count(&self) -> usize {
        self.sealed_chunks.load(Ordering::Acquire) as usize
    }

    /// Sealed chunk `no` (must be `< chunk_count()`).
    #[inline]
    pub fn chunk(&self, no: usize) -> Option<&Chunk<T>> {
        self.dir
            .get(no / SEG_CHUNKS)?
            .get()?
            .get(no % SEG_CHUNKS)?
            .get()
    }

    /// Number of the sealed chunk that holds global index `idx`.
    pub fn chunk_no_of(&self, idx: u32) -> Option<usize> {
        let n = self.chunk_count();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.chunk(mid)?.base <= idx {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let no = lo.checked_sub(1)?;
        let c = self.chunk(no)?;
        ((idx - c.base) < c.len() as u32).then_some(no)
    }

    /// Chunk and local position of global index `idx`.
    pub fn locate(&self, idx: u32) -> Option<(&Chunk<T>, usize)> {
        let c = self.chunk(self.chunk_no_of(idx)?)?;
        Some((c, (idx - c.base) as usize))
    }

    /// Approximate heap bytes held by sealed chunks plus the open builder
    /// (excluding whatever `T` owns on the heap).
    pub fn heap_bytes(&self) -> usize {
        let mut total = self.dir.len() * std::mem::size_of::<OnceLock<Segment<T>>>();
        for no in 0..self.chunk_count() {
            if let Some(c) = self.chunk(no) {
                total += c.bytes.len()
                    + c.ends.len() * 4
                    + c.masks.len() * std::mem::size_of::<Mask>()
                    + c.boosts.len() * 2
                    + c.items.len() * std::mem::size_of::<T>();
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_marks_boundaries_and_non_ascii() {
        let m = mask_of(b"src/Main_file.rs");
        assert!(m.any & char_bit(b'm') != 0);
        assert!(m.any & char_bit(b'M') != 0, "case folded");
        // `m` follows a `/`, `f` follows `_`, `a` is inside a word.
        assert!(m.boundary & char_bit(b'm') != 0);
        assert!(m.boundary & char_bit(b'f') != 0);
        assert!(m.boundary & char_bit(b'a') == 0);
        let u = mask_of("é/x".as_bytes());
        assert!(u.any & NON_ASCII != 0);
        assert_eq!(u.any, u64::MAX);
        assert_eq!(mask_of(b"abc").any & NON_ASCII, 0);
    }

    #[test]
    fn store_round_trips_across_chunks() {
        let s: Store<usize> = Store::new();
        let n = CHUNK_ITEMS * 2 + 17;
        for i in 0..n {
            s.push(i, &format!("dir/file_{i}.rs"), (i % 7) as u16);
        }
        assert_eq!(s.injected() as usize, n);
        assert_eq!(s.pending(), 17);
        s.flush();
        assert_eq!(s.pending(), 0);
        s.push(n, "tail.rs", 0);
        s.flush();
        for i in [0usize, 1, CHUNK_ITEMS - 1, CHUNK_ITEMS, n - 1, n] {
            let (c, j) = s.locate(i as u32).expect("located");
            assert_eq!(c.items[j], i);
            if i < n {
                assert_eq!(c.hay(j), format!("dir/file_{i}.rs").as_bytes());
                assert_eq!(c.boosts[j], (i % 7) as u16);
            }
        }
        assert!(s.locate(n as u32 + 1).is_none());
        assert!(s.heap_bytes() > 0);
    }

    #[test]
    fn long_haystacks_are_truncated_on_char_boundaries() {
        let s: Store<u8> = Store::new();
        let long = "é".repeat(MAX_HAYSTACK);
        s.push(1, &long, 0);
        s.flush();
        let (c, j) = s.locate(0).expect("located");
        assert!(c.hay(j).len() <= MAX_HAYSTACK);
        assert!(std::str::from_utf8(c.hay(j)).is_ok());
    }
}
