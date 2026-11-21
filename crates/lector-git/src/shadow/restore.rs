//! Helpers of restoring: removing emptied directories, writing restored files, intent-to-add.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

use crate::error::{Error, Result};

use super::command::exec;
use super::util::{apply_permissions, make_symlink};
use super::ShadowRepo;

impl ShadowRepo {
    pub(super) fn remove_empty_parents(&self, file: &Path) {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d == self.workdir || !d.starts_with(&self.workdir) {
                break;
            }
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// Atomically writes one restored file (temp file in the same directory + rename).
    pub(super) fn write_restored(
        &self,
        rel: &str,
        mode: &str,
        reader: &mut dyn Read,
        size: u64,
    ) -> Result<()> {
        let abs = self.workdir.join(rel);
        let parent = abs
            .parent()
            .ok_or_else(|| Error::Parse(format!("bad path {rel:?}")))?;
        fs::create_dir_all(parent)?;
        let existing = fs::symlink_metadata(&abs).ok();
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let tmp = parent.join(format!(
            ".lector-restore-{}-{}.tmp",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            if mode == "120000" {
                let mut target = Vec::with_capacity(size as usize);
                reader.read_to_end(&mut target)?;
                make_symlink(&target, &tmp)?;
            } else {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                io::copy(reader, &mut file)?;
                file.sync_all()?;
                drop(file);
                apply_permissions(&tmp, existing.as_ref(), mode == "100755")?;
            }
            fs::rename(&tmp, &abs)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// The paths among (or below) `rels` whose index entry is intent-to-add: the only entries
    /// the index-to-working-tree diff shows as added.
    pub(super) fn intent_to_add(&self, rels: &[String]) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for chunk in rels.chunks(500) {
            let mut args = vec![
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules",
                "--diff-filter=A",
                "--",
            ];
            args.extend(chunk.iter().map(String::as_str));
            let output = exec(self.user_git(&args), "git diff", None)?;
            for name in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                out.insert(String::from_utf8_lossy(name).into_owned());
            }
        }
        Ok(out)
    }
}
