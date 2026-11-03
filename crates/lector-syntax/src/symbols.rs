//! Workspace-wide symbol index (fallback navigation without an LSP).

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;

use crate::input::{Point, TextInput};
use crate::outline::SymbolKind;
use crate::tags::Tag;

/// Where a symbol definition or reference lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolLocation {
    /// File path (as given to [`SymbolIndex::insert_file`]).
    pub path: PathBuf,
    /// Symbol name.
    pub name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Byte range of the name.
    pub range: Range<usize>,
    /// Row/column of the start of the name.
    pub point: Point,
}

#[derive(Default)]
struct Inner {
    definitions: HashMap<String, Vec<SymbolLocation>>,
    references: HashMap<String, Vec<SymbolLocation>>,
    /// Names indexed per file, to make removal cheap.
    files: HashMap<PathBuf, Vec<String>>,
}

impl Inner {
    fn remove(&mut self, path: &Path) {
        let Some(names) = self.files.remove(path) else {
            return;
        };
        for name in names {
            for map in [&mut self.definitions, &mut self.references] {
                if let Some(v) = map.get_mut(&name) {
                    v.retain(|l| l.path != path);
                    if v.is_empty() {
                        map.remove(&name);
                    }
                }
            }
        }
    }
}

/// Thread-safe index of tags keyed by symbol name.
#[derive(Default)]
pub struct SymbolIndex {
    inner: RwLock<Inner>,
}

fn sorted(mut v: Vec<SymbolLocation>) -> Vec<SymbolLocation> {
    v.sort_by(|a, b| (&a.path, a.range.start).cmp(&(&b.path, b.range.start)));
    v
}

impl SymbolIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Index (replacing any previous entries for) `path` from its `tags`.
    pub fn insert_file<T: TextInput + ?Sized>(&self, path: PathBuf, text: &T, tags: Vec<Tag>) {
        let mut entries: Vec<(bool, SymbolLocation)> = tags
            .into_iter()
            .map(|t| {
                let point = text.offset_to_point(t.range.start);
                (
                    t.is_definition,
                    SymbolLocation {
                        path: path.clone(),
                        name: t.name,
                        kind: t.kind,
                        range: t.range,
                        point,
                    },
                )
            })
            .collect();
        entries.sort_by_key(|(_, l)| l.range.start);
        let mut inner = self.inner.write();
        inner.remove(&path);
        let mut names = Vec::new();
        for (is_def, loc) in entries {
            names.push(loc.name.clone());
            let map = if is_def {
                &mut inner.definitions
            } else {
                &mut inner.references
            };
            map.entry(loc.name.clone()).or_default().push(loc);
        }
        names.sort();
        names.dedup();
        inner.files.insert(path, names);
    }

    /// Forget everything indexed for `path`.
    pub fn remove_file(&self, path: &Path) {
        self.inner.write().remove(path);
    }

    /// Definitions named exactly `name`, ordered by path then position.
    pub fn definitions(&self, name: &str) -> Vec<SymbolLocation> {
        sorted(
            self.inner
                .read()
                .definitions
                .get(name)
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// References to `name`, ordered by path then position.
    pub fn references(&self, name: &str) -> Vec<SymbolLocation> {
        sorted(
            self.inner
                .read()
                .references
                .get(name)
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// Every indexed definition (for the symbol picker), ordered by path then position.
    pub fn all_definitions(&self) -> Vec<SymbolLocation> {
        let inner = self.inner.read();
        sorted(inner.definitions.values().flatten().cloned().collect())
    }

    /// Definitions indexed for one file, in document order.
    pub fn definitions_in_file(&self, path: &Path) -> Vec<SymbolLocation> {
        let inner = self.inner.read();
        let Some(names) = inner.files.get(path) else {
            return Vec::new();
        };
        let mut out: Vec<SymbolLocation> = names
            .iter()
            .filter_map(|n| inner.definitions.get(n))
            .flatten()
            .filter(|l| l.path == path)
            .cloned()
            .collect();
        out.sort_by_key(|l| l.range.start);
        out
    }

    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.inner.read().files.len()
    }
}
