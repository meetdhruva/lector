//! Small helpers: relative-path validation, ref and `ls-files` parsing, quoting, and the
//! platform-specific file operations (executable bit, symlinks, permissions).

use std::path::{Component, Path};
use std::{fs, io};

use crate::error::{Error, Result};
use crate::Oid;

use super::{IndexRecord, REF_PREFIX};

/// Normalizes a repository-relative path and refuses anything that could leave the work
/// tree or reach into a `.git` directory.
pub(super) fn check_rel(path: &str) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| Error::Parse(format!("path is not valid UTF-8: {path:?}")))?;
                if name.eq_ignore_ascii_case(".git") {
                    return Err(Error::Parse(format!(
                        "refusing to touch a .git directory: {path:?}"
                    )));
                }
                parts.push(name.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Parse(format!(
                    "refusing to use a path outside the working tree: {path:?}"
                )))
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::Parse(format!("empty path: {path:?}")));
    }
    Ok(parts.join("/"))
}

/// `path == base` or `path` lies below the directory `base`.
pub(super) fn is_within(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(super) fn sanitize_note(note: &str) -> String {
    let single: String = note
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    single.trim().to_owned()
}

pub(super) fn ref_time(refname: &str) -> Option<i64> {
    refname
        .strip_prefix(REF_PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// C-style quoting for fast-import / hash-object paths when needed.
pub(super) fn quote_path(path: &str) -> String {
    let needs = path
        .bytes()
        .any(|b| b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f);
    if !needs {
        return path.to_owned();
    }
    let mut out = String::from("\"");
    for ch in path.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(super) fn parse_ls_files_stage(record: &[u8]) -> Result<IndexRecord> {
    let bad = || {
        Error::Parse(format!(
            "unexpected ls-files output: {:?}",
            String::from_utf8_lossy(record)
        ))
    };
    let tab = record.iter().position(|b| *b == b'\t').ok_or_else(bad)?;
    let meta = String::from_utf8_lossy(&record[..tab]).into_owned();
    let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
    let mut it = meta.split(' ');
    let (Some(mode), Some(oid), Some(stage)) = (it.next(), it.next(), it.next()) else {
        return Err(bad());
    };
    Ok(IndexRecord {
        path,
        mode: mode.to_owned(),
        oid: Oid::from_hex(oid).ok_or_else(bad)?,
        stage: stage.parse().map_err(|_| bad())?,
    })
}

pub(super) fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(unix)]
pub(super) fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(super) fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub(super) fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(super) fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}

#[cfg(unix)]
pub(super) fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), link)
}

#[cfg(not(unix))]
pub(super) fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    // No portable symlink creation: keep the link target as the file's content.
    fs::write(link, target)
}

/// Keeps the permission bits of the file being replaced; otherwise the freshly created
/// file's. Only the executable bits follow the recorded mode.
#[cfg(unix)]
pub(super) fn apply_permissions(
    tmp: &Path,
    existing: Option<&fs::Metadata>,
    executable: bool,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut mode = match existing {
        Some(meta) if meta.is_file() => meta.permissions().mode() & 0o7777,
        _ => fs::metadata(tmp)?.permissions().mode() & 0o7777,
    };
    let has_exec = mode & 0o111 != 0;
    if executable && !has_exec {
        // Like git: x wherever r is.
        mode |= (mode & 0o444) >> 2;
    } else if !executable && has_exec {
        mode &= !0o111;
    }
    fs::set_permissions(tmp, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(super) fn apply_permissions(
    tmp: &Path,
    existing: Option<&fs::Metadata>,
    _executable: bool,
) -> io::Result<()> {
    if let Some(meta) = existing {
        if meta.is_file() {
            fs::set_permissions(tmp, meta.permissions())?;
        }
    }
    Ok(())
}

/// Whether two existing paths are the same directory entry (case-insensitive or normalising
/// file systems give one file several spellings). `false` when either does not exist.
#[cfg(unix)]
pub(super) fn same_entry(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Whether two existing paths are the same directory entry (see the Unix version): compared by
/// their canonical paths, which Windows returns in the stored spelling.
#[cfg(not(unix))]
pub(super) fn same_entry(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}
