//! Logging: everything goes to stderr and to `lector_config::paths::log_file()`
//! (`workspace::OpenLogFile` opens it). The file is capped: when it exceeds
//! [`MAX_LOG_BYTES`] it is renamed to `lector.log.old` (replacing the previous
//! one) and a fresh file is started, at startup and while running.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Size at which the log file is rotated.
pub const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// A size-capped log file with one backup.
pub struct LogFile {
    path: PathBuf,
    state: Mutex<Option<(File, u64)>>,
    max_bytes: u64,
}

impl LogFile {
    /// Opens (appending) `path`, rotating it first if it is already too big.
    pub fn open(path: PathBuf, max_bytes: u64) -> io::Result<LogFile> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size >= max_bytes {
            rotate(&path)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(LogFile {
            path,
            state: Mutex::new(Some((file, size))),
            max_bytes,
        })
    }

    fn write_all(&self, buf: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some((_, size)) = state.as_ref() {
            if size + buf.len() as u64 > self.max_bytes {
                *state = None;
                let reopened = rotate(&self.path).and_then(|()| {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)
                });
                if let Ok(file) = reopened {
                    *state = Some((file, 0));
                }
            }
        }
        if let Some((file, size)) = state.as_mut() {
            if file.write_all(buf).is_ok() {
                *size += buf.len() as u64;
            }
        }
    }
}

/// Moves `path` to `<path>.old`.
fn rotate(path: &Path) -> io::Result<()> {
    let mut old = path.as_os_str().to_owned();
    old.push(".old");
    let old = PathBuf::from(old);
    let _ = std::fs::remove_file(&old);
    match std::fs::rename(path, &old) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Writer handed to `tracing_subscriber`: stderr plus the log file.
pub struct TeeWriter(Option<Arc<LogFile>>);

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(file) = &self.0 {
            file.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// Installs the global subscriber (`RUST_LOG` overrides the default filter).
pub fn init() {
    let file = match LogFile::open(lector_config::paths::log_file(), MAX_LOG_BYTES) {
        Ok(f) => Some(Arc::new(f)),
        Err(err) => {
            eprintln!(
                "lector: cannot open log file {}: {err}",
                lector_config::paths::log_file().display()
            );
            None
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,lector=info")),
        )
        .with_ansi(false)
        .with_writer(move || TeeWriter(file.clone()))
        .init();
    tracing::info!(
        "Lector {} starting ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lector.log");
        let log = LogFile::open(path.clone(), 100).unwrap();
        log.write_all(&[b'a'; 60]);
        log.write_all(&[b'b'; 60]);
        let old = dir.path().join("lector.log.old");
        assert_eq!(std::fs::read(&old).unwrap(), vec![b'a'; 60]);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'b'; 60]);
        drop(log);
        // Oversized at startup: rotated before appending.
        std::fs::write(&path, vec![b'c'; 150]).unwrap();
        let log = LogFile::open(path.clone(), 100).unwrap();
        log.write_all(b"x");
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        assert_eq!(std::fs::read(&old).unwrap().len(), 150);
    }
}
