//! Taking snapshots: planning which files to record and streaming them into `git fast-import`.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use crate::error::{Error, Result};
use crate::pipes::drain;
use crate::Oid;

use super::command::{exec, spawn_error};
use super::util::{is_executable, path_bytes, quote_path, sanitize_note};
use super::{
    now_secs, FileKind, Plan, PlannedFile, ShadowRepo, ShadowSnapshot, BUFFER_LIMIT, REF_PREFIX,
};

impl ShadowRepo {
    /// Stats `rel` and adds it (or, for directories when `expand`, everything below) to `plan`.
    pub(super) fn collect(
        &self,
        rel: &str,
        plan: &mut Plan,
        expand: bool,
        max_file: u64,
    ) -> Result<()> {
        let abs = self.workdir.join(rel);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                plan.absent.push(rel.to_owned());
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        let ty = meta.file_type();
        if ty.is_symlink() {
            plan.files.push(PlannedFile {
                rel: rel.to_owned(),
                kind: FileKind::Link,
                size: 0,
            });
        } else if ty.is_file() {
            if meta.len() > max_file {
                plan.skipped.push(rel.to_owned());
            } else {
                plan.files.push(PlannedFile {
                    rel: rel.to_owned(),
                    kind: FileKind::File {
                        executable: is_executable(&meta),
                    },
                    size: meta.len(),
                });
            }
        } else if ty.is_dir() && expand {
            let mut names: Vec<String> = fs::read_dir(&abs)?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.eq_ignore_ascii_case(".git"))
                .collect();
            names.sort();
            for name in names {
                self.collect(&format!("{rel}/{name}"), plan, true, max_file)?;
            }
        }
        // Sockets, fifos, devices and (when not expanding) directories: nothing to record.
        Ok(())
    }

    /// Refuses paths that reach the working tree through a symlinked directory.
    pub(super) fn no_symlink_parents(&self, rel: &str) -> Result<()> {
        let mut current = self.workdir.clone();
        let comps: Vec<&str> = rel.split('/').collect();
        for comp in &comps[..comps.len().saturating_sub(1)] {
            current.push(comp);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(Error::Parse(format!(
                        "{rel:?} goes through a symbolic link; refusing"
                    )))
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Streams a whole snapshot (blobs, trees, commit, ref) into the shadow through one
    /// `git fast-import`; the ref only appears if the import succeeds.
    pub(super) fn write_snapshot(&self, mut plan: Plan, note: &str) -> Result<ShadowSnapshot> {
        let _lock = self.lock()?;
        let note = sanitize_note(note);
        let time = now_secs();
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let nonce = format!(
            "{time}-{:08x}{:04x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0),
            NONCE.fetch_add(1, Ordering::Relaxed) & 0xffff
        );
        let ref_name = format!("{REF_PREFIX}{nonce}-{}", std::process::id());

        let mut cmd = self.shadow_git();
        cmd.args(["fast-import", "--quiet", "--done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let stderr = drain(child.stderr.take());
        let result = (|| -> Result<()> {
            let mut w = io::BufWriter::new(
                child
                    .stdin
                    .take()
                    .ok_or_else(|| Error::Git("fast-import has no stdin".into()))?,
            );
            writeln!(w, "commit {ref_name}")?;
            writeln!(w, "committer Lector <lector@localhost> {time} +0000")?;
            writeln!(w, "data {}", note.len())?;
            w.write_all(note.as_bytes())?;
            writeln!(w)?;

            let mut present: Vec<&PlannedFile> = Vec::new();
            for file in &plan.files {
                if self.stream_file(&mut w, file)? {
                    present.push(file);
                } else {
                    plan.vanished.push(file.rel.clone());
                }
            }
            // Entries carried over from other snapshots: their blobs are already here.
            for (rel, mode, oid) in &plan.recorded {
                writeln!(w, "M {mode} {oid} {}", quote_path(&format!("files/{rel}")))?;
            }
            let mut blobs = BTreeSet::new();
            for entry in &plan.index {
                if entry.mode != "160000" && blobs.insert(entry.oid) {
                    writeln!(w, "M 100644 {} blobs/{}", entry.oid, entry.oid)?;
                }
            }
            let manifest = plan.manifest();
            writeln!(w, "M 100644 inline manifest")?;
            writeln!(w, "data {}", manifest.len())?;
            w.write_all(&manifest)?;
            writeln!(w)?;
            writeln!(w, "done")?;
            w.flush()?;
            Ok(())
        })();
        let output = if result.is_err() {
            let _ = child.kill();
            child.wait_with_output()
        } else {
            child.wait_with_output()
        };
        let output = output?;
        let stderr_text = stderr.join();
        if let Err(err) = result {
            // A broken pipe means fast-import died first; its message explains why.
            if !stderr_text.trim().is_empty() {
                return Err(Error::GitCommand {
                    command: "fast-import".into(),
                    status: output.status.to_string(),
                    stderr: stderr_text.trim().to_owned(),
                });
            }
            return Err(err);
        }
        if !output.status.success() {
            return Err(Error::GitCommand {
                command: "fast-import".into(),
                status: output.status.to_string(),
                stderr: stderr_text.trim().to_owned(),
            });
        }
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args(["rev-parse", "--verify", "-q", &ref_name]);
                c
            },
            "git rev-parse",
            None,
        )?;
        let commit = Oid::from_hex(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| Error::Parse("fast-import produced no commit".into()))?;
        let snapshot = ShadowSnapshot { commit, time, note };
        self.consolidate_if_needed();
        Ok(snapshot)
    }

    /// Writes `M <mode> inline files/<rel>` + data. `Ok(false)` if the file vanished.
    fn stream_file(&self, w: &mut impl Write, file: &PlannedFile) -> Result<bool> {
        let abs = self.workdir.join(&file.rel);
        let tree_path = quote_path(&format!("files/{}", file.rel));
        match file.kind {
            FileKind::Link => {
                let target = match fs::read_link(&abs) {
                    Ok(t) => t,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(err) => return Err(err.into()),
                };
                let bytes = path_bytes(&target);
                writeln!(w, "M 120000 inline {tree_path}")?;
                writeln!(w, "data {}", bytes.len())?;
                w.write_all(&bytes)?;
                writeln!(w)?;
                Ok(true)
            }
            FileKind::File { executable } => {
                let mut handle = match fs::File::open(&abs) {
                    Ok(f) => f,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(err) => return Err(err.into()),
                };
                let mode = if executable { "100755" } else { "100644" };
                let len = handle.metadata()?.len();
                writeln!(w, "M {mode} inline {tree_path}")?;
                if len <= BUFFER_LIMIT {
                    let mut data = Vec::with_capacity(len as usize);
                    handle.read_to_end(&mut data)?;
                    writeln!(w, "data {}", data.len())?;
                    w.write_all(&data)?;
                } else {
                    writeln!(w, "data {len}")?;
                    let copied = io::copy(&mut (&mut handle).take(len), w)?;
                    if copied != len {
                        return Err(Error::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("{} changed while it was being recorded", file.rel),
                        )));
                    }
                }
                writeln!(w)?;
                Ok(true)
            }
        }
    }
}
