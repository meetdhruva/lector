//! Local crash reports. Nothing here ever leaves the machine.
//!
//! [`install_panic_hook`] chains a hook in front of the previous one that writes a small text
//! report to `<data dir>/crashes/`: time, version, OS/architecture, thread, panic message,
//! location and backtrace. Privacy rules, enforced by [`render_report`]:
//!
//! * the user's home directory (and any `/home/<name>`, `/Users/<name>`, `C:\Users\<name>`)
//!   shows as `~`;
//! * only the first line of the panic message is kept, cut to 300 characters, and the text a
//!   failed string slice quotes is dropped: messages of `assert_eq!` and friends embed the
//!   compared values, which could be file contents;
//! * no document, buffer or settings content is ever collected.
//!
//! The hook must never panic (a panic inside a panic hook aborts the process), so everything it
//! calls returns `Result`s that it ignores, and it works on an unwritable data directory.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{rfc3339, unix_secs};

/// How many reports are kept (the newest).
pub const MAX_REPORTS: usize = 20;
/// Largest report in bytes; the backtrace is cut to fit.
pub const MAX_REPORT_BYTES: usize = 48 * 1024;
/// Reports written per process: a panic loop must not fill the disk (panics are still counted).
pub const MAX_REPORTS_PER_PROCESS: usize = 5;
/// Longest kept panic message, in characters.
pub const MAX_MESSAGE_CHARS: usize = 300;

const FILE_PREFIX: &str = "crash-";
const FILE_SUFFIX: &str = ".txt";
const HEADER: &str = "Lector crash report (v1)";

static INSTALLED: AtomicBool = AtomicBool::new(false);
static PANICS: AtomicUsize = AtomicUsize::new(0);
static SEQ: AtomicUsize = AtomicUsize::new(0);

/// Where and how the hook writes.
#[derive(Debug, Clone)]
pub struct HookOptions {
    /// Directory for the reports (`<data dir>/crashes`).
    pub dir: PathBuf,
    /// Number of reports kept.
    pub keep: usize,
    /// Reports written per process.
    pub max_per_process: usize,
    /// Version shown in the report.
    pub version: String,
    /// The home directory to redact (default: the user's).
    pub home: Option<PathBuf>,
}

impl Default for HookOptions {
    fn default() -> Self {
        HookOptions {
            dir: super::HealthPaths::default().crashes,
            keep: MAX_REPORTS,
            max_per_process: MAX_REPORTS_PER_PROCESS,
            version: env!("CARGO_PKG_VERSION").to_string(),
            home: home_dir(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Installs the crash-report hook with the default options (the user's data directory).
/// Returns `false` when a hook of ours is already installed (installing twice is harmless).
pub fn install_panic_hook() -> bool {
    install_panic_hook_with(HookOptions::default())
}

/// Installs the crash-report hook with explicit options. Chains to the previously installed
/// hook, which runs after the report is written.
pub fn install_panic_hook_with(options: HookOptions) -> bool {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return false;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(&options, info);
        previous(info);
    }));
    true
}

/// The number of panics seen by the hook in this process (including those past the report cap).
pub fn panics_in_process() -> usize {
    PANICS.load(Ordering::Relaxed)
}

/// Everything the report says about one panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicDetails {
    /// The panic message (unsanitized).
    pub message: String,
    /// `file:line:column` of the panic.
    pub location: Option<String>,
    /// Name of the panicking thread.
    pub thread: Option<String>,
    /// Rendered backtrace.
    pub backtrace: String,
}

/// Facts about the process that go into every report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportContext {
    /// Lector version.
    pub version: String,
    /// Seconds since the Unix epoch.
    pub time: u64,
    /// The active session's id, when a session was begun.
    pub session: Option<String>,
    /// The home directory to redact.
    pub home: Option<PathBuf>,
}

fn record_panic(options: &HookOptions, info: &PanicHookInfo<'_>) {
    let count = PANICS.fetch_add(1, Ordering::SeqCst) + 1;
    if count > options.max_per_process {
        super::session::note_panic(count);
        return;
    }
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-string panic payload)".to_string());
    let details = PanicDetails {
        message,
        location: info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
        thread: std::thread::current().name().map(str::to_string),
        backtrace: std::backtrace::Backtrace::force_capture().to_string(),
    };
    let cx = ReportContext {
        version: options.version.clone(),
        time: unix_secs(),
        session: super::session::current_session_id(),
        home: options.home.clone(),
    };
    let text = render_report(&details, &cx);
    let _ = write_report(&options.dir, cx.time, &text, options.keep);
    super::session::note_panic(count);
}

/// Renders a report. Pure: used by the hook and by the tests.
pub fn render_report(details: &PanicDetails, cx: &ReportContext) -> String {
    let redact_text = |s: &str| redact(s, cx.home.as_deref());
    let mut out = String::new();
    let _ = writeln!(out, "{HEADER}");
    let _ = writeln!(out, "version: {}", cx.version);
    let _ = writeln!(out, "time: {}", rfc3339(cx.time));
    let _ = writeln!(
        out,
        "os: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let _ = writeln!(out, "thread: {}", details.thread.as_deref().unwrap_or("?"));
    if let Some(session) = &cx.session {
        let _ = writeln!(out, "session: {session}");
    }
    if let Some(location) = &details.location {
        let _ = writeln!(out, "location: {}", redact_text(location));
    }
    let _ = writeln!(
        out,
        "message: {}",
        redact_text(&sanitize_message(&details.message))
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "This report stays on this computer. It contains no file contents; your home folder is shown as ~."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "backtrace:");
    let backtrace = redact_text(&details.backtrace);
    let room = MAX_REPORT_BYTES.saturating_sub(out.len() + 32);
    if backtrace.len() > room {
        let mut cut = room;
        while cut > 0 && !backtrace.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(backtrace.get(..cut).unwrap_or(""));
        out.push_str("\n[truncated]\n");
    } else {
        out.push_str(&backtrace);
    }
    out
}

/// First line of the message, cut to [`MAX_MESSAGE_CHARS`], without the text a failed string
/// slice quotes (`... of `<the whole string>``).
pub fn sanitize_message(message: &str) -> String {
    let first = message.lines().next().unwrap_or("");
    let first = match first.find(" of `") {
        Some(at) if first.contains("char boundary") || first.contains("out of bounds") => {
            first.get(..at).unwrap_or(first)
        }
        _ => first,
    };
    let mut out: String = first.chars().take(MAX_MESSAGE_CHARS).collect();
    if first.chars().count() > MAX_MESSAGE_CHARS {
        out.push('…');
    }
    out
}

/// Replaces the home directory and other users' home folders in `text` with `~`.
pub fn redact(text: &str, home: Option<&Path>) -> String {
    let mut out = text.to_string();
    if let Some(home) = home {
        let home = home.to_string_lossy();
        let home = home.trim_end_matches(['/', '\\']);
        // A home of "/" (or empty) would redact everything.
        if home.len() > 1 {
            let slash = home.replace('\\', "/");
            let back = home.replace('/', "\\");
            let mut variants = vec![home.to_string(), slash, back];
            variants.dedup();
            for variant in variants {
                out = replace_path_prefix(&out, &variant);
            }
        }
    }
    redact_user_dirs(&out)
}

/// Replaces `prefix` with `~` where it is followed by a path separator or the end of a word.
fn replace_path_prefix(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let (before, after) = rest.split_at(at);
        let tail = after.get(prefix.len()..).unwrap_or("");
        out.push_str(before);
        let boundary = tail
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'));
        out.push_str(if boundary { "~" } else { prefix });
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// `/home/<name>`, `/Users/<name>` and `X:\Users\<name>` (either slash) become `~`.
fn redact_user_dirs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((skip, strip_drive)) = user_dir_at(text, bytes, i) {
            // Drop the drive letter and colon already copied for a Windows path.
            if strip_drive {
                out.truncate(out.len().saturating_sub(2));
            }
            out.push('~');
            i = skip;
            continue;
        }
        let Some(c) = text.get(i..).and_then(|s| s.chars().next()) else {
            break;
        };
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// When a user directory starts at byte `i`, the index just past the user name, and whether a
/// preceding `X:` drive prefix (already copied to the output) belongs to it.
fn user_dir_at(text: &str, bytes: &[u8], i: usize) -> Option<(usize, bool)> {
    let rest = text.get(i..)?;
    let has_drive = i >= 2
        && bytes.get(i - 1) == Some(&b':')
        && bytes.get(i - 2).is_some_and(u8::is_ascii_alphabetic);
    let head = rest.get(..7).map(str::to_ascii_lowercase);
    let (marker_len, drive) = if rest.starts_with("/home/") {
        (6, false)
    } else if matches!(head.as_deref(), Some("/users/" | "\\users\\")) {
        if has_drive {
            (7, true)
        } else if rest.starts_with("/Users/") {
            (7, false)
        } else {
            return None;
        }
    } else {
        return None;
    };
    let name_len = rest
        .get(marker_len..)?
        .chars()
        .take_while(|c| {
            !matches!(
                c,
                '/' | '\\'
                    | ' '
                    | '\t'
                    | '\r'
                    | '\n'
                    | '"'
                    | '\''
                    | '`'
                    | ':'
                    | ';'
                    | ','
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '['
                    | ']'
            )
        })
        .map(char::len_utf8)
        .sum::<usize>();
    (name_len > 0).then_some((i + marker_len + name_len, drive))
}

/// `crash-20261113T102233Z-<pid>-<n>.txt` (sorts chronologically).
fn file_name(time: u64) -> String {
    let stamp = rfc3339(time).replace(['-', ':'], "");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{FILE_PREFIX}{stamp}-{}-{seq}{FILE_SUFFIX}",
        std::process::id()
    )
}

/// Writes `text` as a new report in `dir` (created if needed) and prunes the directory to the
/// newest `keep` reports. The file appears atomically.
pub fn write_report(dir: &Path, time: u64, text: &str, keep: usize) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let name = file_name(time);
    let tmp = dir.join(format!(".{name}.tmp"));
    let path = dir.join(&name);
    fs::write(&tmp, text)?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    prune_reports(dir, keep);
    Ok(path)
}

/// Report files of `dir`, newest first.
pub fn report_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(FILE_PREFIX) && n.ends_with(FILE_SUFFIX))
        })
        .collect();
    files.sort();
    files.reverse();
    files
}

/// Deletes all but the newest `keep` reports in `dir`.
pub fn prune_reports(dir: &Path, keep: usize) {
    for old in report_files(dir).into_iter().skip(keep) {
        let _ = fs::remove_file(old);
    }
}

/// A crash report as listed by [`super::health_summary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashFile {
    /// Full path of the report.
    pub path: PathBuf,
    /// File name.
    pub file: String,
    /// `time:` header (UTC, RFC 3339).
    pub time: Option<String>,
    /// `version:` header.
    pub version: Option<String>,
    /// `os:` header.
    pub os: Option<String>,
    /// `thread:` header.
    pub thread: Option<String>,
    /// `location:` header.
    pub location: Option<String>,
    /// `message:` header.
    pub message: Option<String>,
}

/// Reads the header of a report; `None` when the file cannot be read.
pub fn read_report_header(path: &Path) -> Option<CrashFile> {
    let text = fs::read_to_string(path).ok()?;
    let mut crash = CrashFile {
        path: path.to_path_buf(),
        file: path.file_name()?.to_string_lossy().into_owned(),
        time: None,
        version: None,
        os: None,
        thread: None,
        location: None,
        message: None,
    };
    for line in text.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        let value = Some(value.to_string());
        match key {
            "time" => crash.time = value,
            "version" => crash.version = value,
            "os" => crash.os = value,
            "thread" => crash.thread = value,
            "location" => crash.location = value,
            "message" => crash.message = value,
            _ => {}
        }
    }
    Some(crash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details() -> PanicDetails {
        PanicDetails {
            message: "called `Option::unwrap()` on a `None` value".into(),
            location: Some("/home/ada/work/lector/crates/x/src/lib.rs:12:5".into()),
            thread: Some("main".into()),
            backtrace: "   0: lector::boom\n             at /home/ada/work/lector/src/main.rs:3\n"
                .into(),
        }
    }

    fn cx() -> ReportContext {
        ReportContext {
            version: "0.7.0".into(),
            time: 1_700_000_000,
            session: Some("s1".into()),
            home: Some(PathBuf::from("/home/ada")),
        }
    }

    #[test]
    fn report_is_well_formed_and_redacted() {
        let text = render_report(&details(), &cx());
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(HEADER));
        assert_eq!(lines.next(), Some("version: 0.7.0"));
        assert_eq!(lines.next(), Some("time: 2023-11-14T22:13:20Z"));
        assert!(text.contains("\nthread: main\n"));
        assert!(text.contains("\nsession: s1\n"));
        assert!(text.contains("\nlocation: ~/work/lector/crates/x/src/lib.rs:12:5\n"));
        assert!(text.contains("\nmessage: called `Option::unwrap()` on a `None` value\n"));
        assert!(text.contains("\nbacktrace:\n   0: lector::boom\n"));
        assert!(!text.contains("ada"), "{text}");
        assert!(text.len() <= MAX_REPORT_BYTES);
    }

    #[test]
    fn redaction_covers_home_variants_and_other_users() {
        let home = Some(Path::new("/Users/Grace Hopper"));
        assert_eq!(
            redact("open /Users/Grace Hopper/code/a.rs failed", home),
            "open ~/code/a.rs failed"
        );
        // Not a prefix of a longer name.
        assert_eq!(
            redact(
                "/home/bobby/x and /home/bob/x",
                Some(Path::new("/home/bob"))
            ),
            "~/x and ~/x"
        );
        assert_eq!(
            redact("/home/ann/.cargo/registry", None),
            "~/.cargo/registry"
        );
        assert_eq!(redact("/Users/ann/Library/x", None), "~/Library/x");
        assert_eq!(
            redact(r"C:\Users\Ann\AppData\Local\x and D:/Users/Bo/y", None),
            r"~\AppData\Local\x and ~/y"
        );
        let win_home = Some(Path::new(r"C:\Users\Ann"));
        assert_eq!(redact("C:/Users/Ann/work", win_home), "~/work");
        assert_eq!(redact(r"C:\Users\Ann\work", win_home), r"~\work");
        // Nothing to redact.
        assert_eq!(
            redact("/usr/lib/x /rustc/abc/library", None),
            "/usr/lib/x /rustc/abc/library"
        );
        // A home of "/" must not erase every path.
        assert_eq!(redact("/etc/x", Some(Path::new("/"))), "/etc/x");
        // Non-ASCII survives.
        assert_eq!(redact("wörld /home/zoë/ü", None), "wörld ~/ü");
    }

    #[test]
    fn messages_drop_embedded_values() {
        assert_eq!(sanitize_message("plain"), "plain");
        assert_eq!(
            sanitize_message(
                "assertion `left == right` failed\n  left: \"secret file text\"\n right: \"x\""
            ),
            "assertion `left == right` failed"
        );
        assert_eq!(
            sanitize_message(
                "byte index 3 is not a char boundary; it is inside 'é' (bytes 2..4) of `private text here`"
            ),
            "byte index 3 is not a char boundary; it is inside 'é' (bytes 2..4)"
        );
        let long = "x".repeat(1000);
        let cut = sanitize_message(&long);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(sanitize_message(""), "");
    }

    #[test]
    fn oversized_backtraces_are_truncated_on_a_char_boundary() {
        let mut d = details();
        d.backtrace = "é".repeat(MAX_REPORT_BYTES);
        let text = render_report(&d, &cx());
        assert!(text.len() <= MAX_REPORT_BYTES, "{}", text.len());
        assert!(text.ends_with("[truncated]\n"));
    }

    #[test]
    fn reports_are_written_atomically_and_only_the_newest_are_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let crashes = dir.path().join("crashes");
        let mut paths = Vec::new();
        for i in 0..25u64 {
            let p = write_report(
                &crashes,
                1_700_000_000 + i,
                &format!("{HEADER}\nmessage: n{i}\n"),
                20,
            )
            .expect("write");
            paths.push(p);
        }
        let kept = report_files(&crashes);
        assert_eq!(kept.len(), 20);
        // Newest first; the five oldest are gone.
        assert_eq!(kept[0], paths[24]);
        assert!(!paths[0].exists() && !paths[4].exists());
        assert!(paths[5].exists());
        // No temporary files are left behind, and the header parses.
        let names: Vec<String> = fs::read_dir(&crashes)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| n.starts_with(FILE_PREFIX)),
            "{names:?}"
        );
        let header = read_report_header(&paths[24]).expect("header");
        assert_eq!(header.message.as_deref(), Some("n24"));
    }

    #[test]
    fn an_unwritable_directory_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where the directory should be.
        let blocker = dir.path().join("crashes");
        fs::write(&blocker, "x").expect("write");
        assert!(write_report(&blocker, 1, "x", 20).is_err());
        assert!(write_report(&blocker.join("sub"), 1, "x", 20).is_err());
        assert!(report_files(&blocker).is_empty());
    }

    #[test]
    fn headers_of_foreign_files_do_not_parse_to_anything_surprising() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("crash-x.txt");
        fs::write(&p, "not a report\nrandom: line\n\nmessage: hidden").expect("write");
        let h = read_report_header(&p).expect("header");
        assert_eq!(h.message, None);
        assert_eq!(h.time, None);
        assert!(read_report_header(&dir.path().join("missing.txt")).is_none());
    }
}
