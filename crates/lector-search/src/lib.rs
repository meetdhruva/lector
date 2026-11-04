//! File walking, fuzzy picking (nucleo) and project-wide text search for Lector.
//!
//! * [`walk_files`]: parallel, gitignore-aware directory walk yielding root-relative paths.
//! * [`Picker`]: streaming fuzzy matcher over arbitrary items.
//! * [`search_project`] / [`find_in_text`]: ripgrep-engine text search.
//! * [`replacements_in_text`]: the same matches with their replacement text
//!   (regex capture groups expanded).
//!
//! All blocking APIs are meant to be called from background threads and honour a
//! [`CancelToken`].

#![forbid(unsafe_code)]

mod cancel;
mod globs;
mod grep;
mod picker;
mod walk;

pub use cancel::CancelToken;
pub use grep::{
    find_in_text, replacements_in_text, search_project, CaseMode, FileMatches, LineMatch,
    SearchError, SearchEvent, SearchQuery,
};
pub use picker::{Picker, PickerInjector, PickerMatch};
pub use walk::{walk_files, WalkOptions};

/// Result alias for search operations.
pub type Result<T> = std::result::Result<T, SearchError>;
