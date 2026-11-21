//! Small OS integrations (read-only): reveal a path in the system file manager. The process
//! is started by `lector_app::os` (ADR 0002: UI crates start no processes).

use std::path::Path;

/// Menu label for [`reveal_in_file_manager`] on this platform.
pub const REVEAL_LABEL: &str = lector_app::os::REVEAL_LABEL;

/// Shows `path` in the OS file manager (selects files where supported).
/// Returns at once; failures are logged, never fatal.
pub fn reveal_in_file_manager(path: &Path) {
    lector_app::os::reveal_in_file_manager(&lector_app::AppCore::global(), path);
}
