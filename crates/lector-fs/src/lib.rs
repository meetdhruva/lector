//! File loading, kind/encoding detection and a debounced file watcher for
//! Lector.
//!
//! * [`detect_kind`] classifies a path as text, binary, image or directory.
//! * [`decode`] turns bytes into text (UTF-8, UTF-16 with BOM, windows-1252
//!   fallback).
//! * [`load_file`] loads a file into a [`lector_text::Text`], memory-mapping
//!   large files.
//! * [`watch`] reports debounced file changes and git state changes.
//!
//! * [`write_atomic`] saves bytes atomically; [`stamp`] detects changes on disk.
//! * [`confine`] is the one check that a caller-supplied path lies inside a root (agents, MCP,
//!   command path arguments), and reads or writes such a path without following a link that
//!   appeared after the check.
//!
//! Nothing but [`write_atomic`] and [`confine::Confined::write`] writes, and both run only on an
//! explicit request (a save, an agent's or a command's write).
//!
//! [`load_file`] refuses non-regular files (FIFOs, sockets, devices) with
//! `InvalidInput` instead of blocking on them. The watcher does not follow symlinks (like
//! the walker), so a link to `/` or a symlink loop cannot make it crawl or fail; its sink
//! runs on the watcher's own thread.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod confine;
mod decode;
mod kind;
mod load;
pub mod spelling;
mod watch;
mod write;

pub use confine::{Access, ConfineError, Confined, Confinement, CreateDirs};
pub use decode::{decode, UTF_8};
pub use kind::{detect_kind, has_utf16_bom, is_binary, FileKind, ImageFormat, SNIFF_LEN};
pub use load::{load_file, LoadedFile, BINARY};
pub use spelling::{on_disk_relative, EntryNames, FileSystemNames};
pub use watch::{watch, FsEvent, FsEventKind, WatchEvent, Watcher};
pub use write::{stamp, write_atomic, FileStamp};

/// Re-export of the text type stored in [`LoadedFile`].
pub use lector_text::Text;

/// Errors from this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The OS file watcher failed.
    #[error("file watcher error: {0}")]
    Notify(#[from] notify::Error),
    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result type of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
