//! Navigation of the [`Workspace`]: the back/forward history, the file,
//! outline and symbol finders, go to definition and find references.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{prelude::*, App, SharedString, Window};
use lector_app::project::Location;

use super::location;
use crate::actions::{editor, workspace::*};
use crate::background::{background, view_key, JobKind, Priority};
use crate::components::picker::Picker;
use crate::editor::Reveal;
use crate::views::file_finder::FileFinderDelegate;
use crate::views::references::{ReferencesEvent, ReferencesItem, ReferencesView};
use crate::views::symbol_picker::{SymbolEntry, SymbolPickerDelegate};
use crate::views::workspace::Workspace;

impl Workspace {
    // Navigation history.

    fn current_location(&self, cx: &App) -> Option<Location> {
        let view = self.active_file_view(cx)?;
        let v = view.read(cx);
        let pos = v.code_view().read(cx).cursor();
        Some(location(v.path().to_path_buf(), pos))
    }

    /// Pushes the current location on the back stack (before a jump).
    pub fn record_location(&mut self, cx: &App) {
        if let Some(loc) = self.current_location(cx) {
            self.integ.nav.push(loc);
        }
    }

    /// Big cursor moves within a file (≥ 20 rows) are recorded after the
    /// cursor rests, so GoBack returns to where reading was interrupted.
    pub(super) fn schedule_nav_record(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(start) = self.current_location(cx) else {
            return;
        };
        if self.integ.nav_task.is_some() {
            return;
        }
        self.integ.nav_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.integ.nav_task = None;
                if let Some(now) = this.current_location(cx) {
                    if now.path == start.path && now.point.row.abs_diff(start.point.row) >= 20 {
                        this.integ.nav.push(start);
                    }
                }
            });
        }));
    }

    fn go_to_location(&mut self, loc: Location, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let reveal = Reveal {
            row: loc.point.row,
            cols: loc.point.column as usize..loc.point.column as usize,
            center: true,
        };
        self.open_path_with(loc.path, false, Some(reveal), false, window, cx);
    }

    pub(in crate::views::workspace) fn nav_back(
        &mut self,
        _: &GoBack,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(current) = self.current_location(cx) else {
            return;
        };
        if let Some(loc) = self.integ.nav.back(current) {
            self.go_to_location(loc, window, cx);
        }
    }

    pub(in crate::views::workspace) fn nav_forward(
        &mut self,
        _: &GoForward,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(current) = self.current_location(cx) else {
            return;
        };
        if let Some(loc) = self.integ.nav.forward(current) {
            self.go_to_location(loc, window, cx);
        }
    }

    // Pickers.

    pub(in crate::views::workspace) fn toggle_file_finder(
        &mut self,
        _: &ToggleFileFinder,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let root = self.project.root().to_path_buf();
        let files = self.integ.files.clone();
        let active = self.active_path(cx);
        let recent: Vec<Arc<str>> = self
            .integ
            .recent
            .iter()
            .filter(|p| Some(*p) != active.as_ref())
            .filter_map(|p| self.project.relative_path(p))
            .map(Arc::from)
            .collect();
        let status = self.integ.git_status.clone();
        let this = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = FileFinderDelegate::new(
                root,
                files,
                recent,
                status,
                move |path, line, window, cx| {
                    let reveal = line.map(|(l, c)| {
                        let col = c.unwrap_or(1).saturating_sub(1) as usize;
                        Reveal {
                            row: l.saturating_sub(1),
                            cols: col..col,
                            center: true,
                        }
                    });
                    let _ = this.update(cx, |ws, cx| {
                        ws.open_path_with(path, false, reveal, true, window, cx);
                    });
                },
                cx,
            );
            Picker::new(delegate, window, cx)
        });
    }

    fn open_symbol_entry(
        &mut self,
        entry: SymbolEntry,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let reveal = Reveal {
            row: entry.row,
            cols: entry.cols,
            center: true,
        };
        self.open_path_with(entry.path, false, Some(reveal), true, window, cx);
    }

    pub(in crate::views::workspace) fn toggle_outline_picker(
        &mut self,
        _: &ToggleOutline,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(view) = self.active_file_view(cx) else {
            return;
        };
        let v = view.read(cx);
        let Some(doc) = v.document(cx).cloned() else {
            return;
        };
        let Some(text) = doc.text() else { return };
        let path = v.path().to_path_buf();
        let cursor_row = v.code_view().read(cx).cursor().row;
        let entries: Vec<SymbolEntry> = doc
            .outline()
            .iter()
            .map(|item| {
                let p = text.offset_to_point(item.name_range.start);
                let end = text.offset_to_point(item.name_range.end);
                SymbolEntry {
                    name: item.name.clone().into(),
                    kind: Some(item.kind),
                    detail: SharedString::from(item.kind.label()),
                    depth: item.depth,
                    path: path.clone(),
                    row: p.row,
                    cols: p.column as usize..if end.row == p.row {
                        end.column as usize
                    } else {
                        p.column as usize
                    },
                }
            })
            .collect();
        let this = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = SymbolPickerDelegate::new(
                "Go to symbol in file…",
                entries,
                true,
                move |entry, window, cx| {
                    let _ = this.update(cx, |ws, cx| ws.open_symbol_entry(entry, window, cx));
                },
            )
            .with_empty_message("No symbols in this file");
            let mut picker = Picker::new(delegate, window, cx);
            picker.delegate.select_row(cursor_row);
            picker.scroll_to_selected();
            picker
        });
    }

    pub(in crate::views::workspace) fn toggle_symbol_finder(
        &mut self,
        _: &ToggleSymbolFinder,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let project = self.project.clone();
        let indexing = self.integ.index_progress.is_some();
        let this = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let root = project.root().to_path_buf();
            let key = view_key(JobKind::ProjectSymbols, this.entity_id());
            let entries = background(cx, key, Priority::Visible, move |_| {
                lector_app::project::symbols::project_symbols(&project)
                    .into_iter()
                    .map(|s| {
                        let rel = s
                            .path
                            .strip_prefix(&root)
                            .unwrap_or(&s.path)
                            .display()
                            .to_string();
                        SymbolEntry {
                            name: s.name.into(),
                            kind: Some(s.kind),
                            detail: rel.into(),
                            depth: 0,
                            path: s.path,
                            row: s.point.row,
                            cols: s.point.column as usize..s.point.column as usize,
                        }
                    })
                    .collect::<Vec<_>>()
            });
            let Ok(entries) = entries.await else { return };
            let _ = this.update_in(cx, |ws, window, cx| {
                let weak = cx.entity().downgrade();
                ws.toggle_modal(window, cx, move |window, cx| {
                    let delegate = SymbolPickerDelegate::new(
                        "Go to symbol in project…",
                        entries,
                        false,
                        move |entry, window, cx| {
                            let _ =
                                weak.update(cx, |ws, cx| ws.open_symbol_entry(entry, window, cx));
                        },
                    )
                    .loading(indexing);
                    Picker::new(delegate, window, cx)
                });
            });
        })
        .detach();
    }

    // Definitions, references.

    fn active_doc_offset(&self, cx: &App) -> Option<(lector_app::Document, usize)> {
        let view = self.active_file_view(cx)?;
        let v = view.read(cx);
        let doc = v.live_document(cx)?.clone();
        let offset = v.cursor_offset(cx)?;
        Some((doc, offset))
    }

    pub(in crate::views::workspace) fn go_to_definition(
        &mut self,
        _: &editor::GoToDefinition,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some((doc, offset)) = self.active_doc_offset(cx) else {
            return;
        };
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let key = view_key(JobKind::LspDefinitions, this.entity_id());
            let targets = background(cx, key, Priority::Visible, move |_| {
                lector_app::project::symbols::definitions(&project, &doc, offset)
            });
            let Ok(targets) = targets.await else { return };
            let targets = targets.unwrap_or_default();
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.show_targets(targets, "Definitions", window, cx)
            });
        })
        .detach();
    }

    fn show_targets(
        &mut self,
        targets: Vec<lector_app::project::Target>,
        title: &str,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        match targets.len() {
            0 => {
                self.flash_status(format!("No {} found", title.to_lowercase()), cx);
            }
            1 => {
                let t = &targets[0];
                let reveal = Reveal {
                    row: t.range.start.row,
                    cols: t.range.start.column as usize..if t.range.end.row == t.range.start.row {
                        t.range.end.column as usize
                    } else {
                        t.range.start.column as usize
                    },
                    center: true,
                };
                self.open_path_with(t.path.clone(), false, Some(reveal), true, window, cx);
            }
            _ => {
                let root = self.project.root().to_path_buf();
                let entries: Vec<SymbolEntry> = targets
                    .into_iter()
                    .map(|t| {
                        let rel = t
                            .path
                            .strip_prefix(&root)
                            .unwrap_or(&t.path)
                            .display()
                            .to_string();
                        SymbolEntry {
                            name: format!("{rel}:{}", t.range.start.row + 1).into(),
                            kind: None,
                            detail: t.preview.trim().to_string().into(),
                            depth: 0,
                            path: t.path,
                            row: t.range.start.row,
                            cols: t.range.start.column as usize..t.range.start.column as usize,
                        }
                    })
                    .collect();
                let weak = cx.entity().downgrade();
                let placeholder = format!("{title}…");
                self.toggle_modal(window, cx, move |window, cx| {
                    let delegate = SymbolPickerDelegate::new(
                        placeholder,
                        entries,
                        true,
                        move |entry, window, cx| {
                            let _ =
                                weak.update(cx, |ws, cx| ws.open_symbol_entry(entry, window, cx));
                        },
                    );
                    Picker::new(delegate, window, cx)
                });
            }
        }
    }

    pub(in crate::views::workspace) fn find_references(
        &mut self,
        _: &editor::FindReferences,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some((doc, offset)) = self.active_doc_offset(cx) else {
            return;
        };
        let word = doc.word_at(offset).map(|(_, w)| w).unwrap_or_default();
        if word.is_empty() {
            return;
        }
        let project = self.project.clone();
        let view = cx.new(|cx| ReferencesView::new(word.clone(), project.clone(), cx));
        let sub = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ReferencesEvent, window, cx| match event {
                ReferencesEvent::Open { path, row, cols } => {
                    let reveal = Reveal {
                        row: *row,
                        cols: cols.clone(),
                        center: true,
                    };
                    this.open_path_in_other_pane(path.clone(), reveal, window, cx);
                }
            },
        );
        self.integ.subscriptions.push(sub);
        view.update(cx, |v, cx| v.load(doc, offset, cx));
        self.active_pane.update(cx, |p, cx| {
            p.add_item(Box::new(ReferencesItem::new(view, word)), false, window, cx)
        });
    }

    /// Opens a location from a results tab: in another pane if there is one,
    /// else in the active pane.
    fn open_path_in_other_pane(
        &mut self,
        path: PathBuf,
        reveal: Reveal,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let panes = self.center.panes();
        if let Some(other) = panes.iter().find(|p| **p != self.active_pane).cloned() {
            self.set_active_pane(other, cx);
        }
        self.open_path_with(path, true, Some(reveal), true, window, cx);
    }
}
