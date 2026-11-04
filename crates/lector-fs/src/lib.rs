//! File loading, kind/encoding detection and a debounced file watcher for
//! Lector.
//!
//! * [`detect_kind`] classifies a path as text, binary, image or directory.
//! * [`decode`] turns bytes into text (UTF-8, UTF-16 with BOM, Latin-1
//!   fallback).
//! * [`load_file`] loads a file into a [`lector_text::Text`], memory-mapping
//!   large files.
//! * [`watch`] reports debounced file changes and git state changes.
//!
//! * [`write_atomic`] saves bytes atomically; [`stamp`] detects changes on disk.
//!
//! Nothing but [`write_atomic`] writes, and it only runs on an explicit save.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod decode;
mod kind;
mod load;
mod watch;
mod write;

pub use decode::{decode, UTF_8};
pub use kind::{detect_kind, has_utf16_bom, is_binary, FileKind, ImageFormat, SNIFF_LEN};
pub use load::{load_file, LoadedFile, BINARY};
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
