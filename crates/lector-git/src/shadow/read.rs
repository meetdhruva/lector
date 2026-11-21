//! Reading what a snapshot holds: manifest, tree listing, batched blob reads and worktree state.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;
use std::{fs, io};

use crate::error::{Error, Result};
use crate::pipes::{drain, feed_while};
use crate::Oid;

use super::command::{exec, spawn_error};
use super::util::{check_rel, is_executable, path_bytes, quote_path};
use super::{Manifest, ShadowRepo, ShadowSnapshot};

impl ShadowRepo {
    pub(super) fn manifest(&self, snap: &ShadowSnapshot) -> Result<Manifest> {
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args(["cat-file", "blob"])
                    .arg(format!("{}:manifest", snap.commit));
                c
            },
            "git cat-file",
            None,
        )
        .map_err(|err| match err {
            Error::GitCommand { .. } => Error::Git(format!(
                "snapshot {} is no longer in the shadow repository (pruned?)",
                snap.commit.short()
            )),
            other => other,
        })?;
        Manifest::parse(&out)
    }

    /// Recorded `files/` tree entries: path -> (mode, blob).
    pub(super) fn tree_files(
        &self,
        snap: &ShadowSnapshot,
    ) -> Result<BTreeMap<String, (String, Oid)>> {
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args(["ls-tree", "-r", "-z", "--full-tree"])
                    .arg(snap.commit.to_string())
                    .args(["--", "files"]);
                c
            },
            "git ls-tree",
            None,
        )?;
        let mut map = BTreeMap::new();
        for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = record.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&record[..tab]).into_owned();
            let name = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
            let mut it = meta.split(' ');
            let (Some(mode), Some(_ty), Some(oid)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Some(rel), Some(oid)) = (name.strip_prefix("files/"), Oid::from_hex(oid)) else {
                continue;
            };
            map.insert(rel.to_owned(), (mode.to_owned(), oid));
        }
        Ok(map)
    }

    /// Streams blobs through one `git cat-file --batch`; `sink(index, reader, size)`.
    pub(super) fn cat_blobs(
        &self,
        oids: &[Oid],
        mut sink: impl FnMut(usize, &mut dyn Read, u64) -> Result<()>,
    ) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        let mut cmd = self.shadow_git();
        cmd.args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let stdin = child.stdin.take();
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let stderr = drain(child.stderr.take());
        let mut reader = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| Error::Git("cat-file has no stdout".into()))?,
        );
        let outcome = feed_while(stdin, input.as_bytes(), || {
            let mut outcome = Ok(());
            for (index, oid) in oids.iter().enumerate() {
                let mut header = String::new();
                if reader.read_line(&mut header)? == 0 {
                    outcome = Err(Error::Git("git cat-file ended early".into()));
                    break;
                }
                let mut parts = header.split_whitespace();
                let (_, Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
                    outcome = Err(Error::Git(format!(
                        "object {oid} is missing from the shadow"
                    )));
                    break;
                };
                let Ok(size) = size.parse::<u64>() else {
                    outcome = Err(Error::Parse(format!("bad cat-file header {header:?}")));
                    break;
                };
                if kind != "blob" {
                    outcome = Err(Error::Parse(format!("{oid} is not a blob")));
                    break;
                }
                let mut limited = (&mut reader).take(size);
                let sunk = sink(index, &mut limited, size);
                // Drain what the sink left, then the trailing newline.
                let _ = io::copy(&mut limited, &mut io::sink());
                let mut newline = [0u8; 1];
                let _ = reader.read_exact(&mut newline);
                if let Err(err) = sunk {
                    outcome = Err(err);
                    break;
                }
            }
            drop(reader);
            let _ = child.kill();
            let _ = child.wait();
            outcome
        });
        let _ = stderr.join();
        outcome
    }

    /// Current `(mode, blob id)` of working-tree paths (`None` = missing), hashed exactly
    /// like a snapshot would record them (no filters). Nothing is written.
    pub(super) fn worktree_state(&self, rels: &[&str]) -> Result<Vec<Option<(String, Oid)>>> {
        let mut result: Vec<Option<(String, Oid)>> = vec![None; rels.len()];
        let mut file_indices = Vec::new();
        let mut file_args = String::new();
        for (i, rel) in rels.iter().enumerate() {
            let rel = check_rel(rel)?;
            self.no_symlink_parents(&rel)?;
            let abs = self.workdir.join(&rel);
            let Ok(meta) = fs::symlink_metadata(&abs) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&abs)?;
                let mut c = self.shadow_git();
                c.args(["hash-object", "--stdin"]);
                let out = exec(c, "git hash-object", Some(&path_bytes(&target)))?;
                if let Some(oid) = Oid::from_hex(String::from_utf8_lossy(&out).trim()) {
                    result[i] = Some(("120000".into(), oid));
                }
            } else if meta.is_file() {
                file_indices.push((i, is_executable(&meta)));
                file_args.push_str(&quote_path(&rel));
                file_args.push('\n');
            }
        }
        if !file_indices.is_empty() {
            let mut c = self.shadow_git();
            c.args(["hash-object", "--no-filters", "--stdin-paths"]);
            let out = exec(c, "git hash-object", Some(file_args.as_bytes()))?;
            let text = String::from_utf8_lossy(&out);
            for ((i, exec_bit), line) in file_indices.into_iter().zip(text.lines()) {
                if let Some(oid) = Oid::from_hex(line) {
                    let mode = if exec_bit { "100755" } else { "100644" };
                    result[i] = Some((mode.into(), oid));
                }
            }
        }
        Ok(result)
    }
}
