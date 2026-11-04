//! GPUI tests for the LSP editing integration: workspace edits on open and
//! unopened files, version checks, diagnostics mapped through edits, and —
//! against the fake server of `lector-lsp` — format (one undo step), code
//! actions and rename.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Entity, Focusable, TestAppContext, VisualTestContext};
use lector_lsp::{DiagnosticSeverity, DocumentDiagnostics, PointUtf16, TextEdit, WorkspaceEdit};
use lector_theme::{Appearance, ThemeRegistry};

use super::DiagnosticsStore;
use crate::actions::editor::{CodeActions, Format, Rename, Undo};
use crate::code_view::Pos;
use crate::editor::FileView;
use crate::theme::{ThemeMode, ThemeState};
use crate::views::Workspace;

fn settings(fake_lsp: bool) -> lector_config::Settings {
    let mut settings = lector_config::Settings::default();
    settings.session.restore = false;
    settings.lsp.enabled = fake_lsp;
    if fake_lsp {
        let script = format!(
            "{}/../lector-lsp/tests/fixtures/fake_lsp.py",
            env!("CARGO_MANIFEST_DIR")
        );
        settings.lsp.servers.insert(
            "rust-analyzer".into(),
            lector_config::ServerOverride {
                command: Some("python3".into()),
                args: Some(vec![script, "--editing".into()]),
                enabled: Some(true),
            },
        );
    }
    settings
}

fn init(cx: &mut TestAppContext, settings: &lector_config::Settings) {
    let settings = settings.clone();
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(crate::code_view::EditorSettings::default());
        cx.set_global(crate::settings::AppSettings {
            settings: Arc::new(settings),
            warnings: Default::default(),
        });
        crate::settings::bind_effective_keymap(cx);
        super::init(cx);
    });
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn fixture(files: &[(&str, &str)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let root = dir.path().canonicalize().unwrap();
    Fixture { _dir: dir, root }
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
    settings: lector_config::Settings,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let project = lector_workspace::Project::open(root, settings).unwrap();
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    cx.run_until_parked();
    (ws, cx)
}

fn open_file(
    ws: &Entity<Workspace>,
    path: PathBuf,
    cx: &mut VisualTestContext,
) -> Entity<FileView> {
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(path, Some(1), Some(1), window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    cx.update(|window, cx| {
        let focus = view.read(cx).code_view().focus_handle(cx);
        window.focus(&focus);
    });
    cx.run_until_parked();
    view
}

fn text_of(view: &Entity<FileView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |v, cx| {
        v.model()
            .read(cx)
            .buffer()
            .expect("editable")
            .read(cx)
            .text()
            .to_string()
    })
}

fn version_of(view: &Entity<FileView>, cx: &mut VisualTestContext) -> u64 {
    view.read_with(cx, |v, cx| {
        v.model()
            .read(cx)
            .buffer()
            .expect("editable")
            .read(cx)
            .text()
            .version()
    })
}

fn pt(row: u32, column: u32) -> PointUtf16 {
    PointUtf16 { row, column }
}

fn edit(range: std::ops::Range<PointUtf16>, text: &str) -> TextEdit {
    TextEdit {
        range,
        new_text: text.into(),
    }
}

/// Runs the executor while waiting (real time) for `f`: language servers
/// answer on their own threads.
fn wait_until(cx: &mut VisualTestContext, mut f: impl FnMut(&mut VisualTestContext) -> bool) {
    for _ in 0..500 {
        cx.run_until_parked();
        if f(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
        cx.executor().advance_clock(Duration::from_millis(200));
    }
    panic!("timed out");
}

#[gpui::test]
fn workspace_edit_applies_to_open_and_unopened_files(cx: &mut TestAppContext) {
    let s = settings(false);
    init(cx, &s);
    let fx = fixture(&[("a.rs", "fn foo() {}\nfoo();\n"), ("b.rs", "use foo;\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root, s);
    let a = open_file(&ws, fx.root.join("a.rs"), cx);
    let b_path = fx.root.join("b.rs");
    let edit = WorkspaceEdit {
        changes: vec![
            (
                fx.root.join("a.rs"),
                vec![
                    edit(pt(0, 3)..pt(0, 6), "bar"),
                    edit(pt(1, 0)..pt(1, 3), "bar"),
                ],
            ),
            (b_path.clone(), vec![edit(pt(0, 4)..pt(0, 7), "bar")]),
        ],
        versions: vec![],
    };
    let task = ws.update_in(cx, |ws, window, cx| {
        super::apply_workspace_edit(ws, edit, window, cx)
    });
    cx.run_until_parked();
    let n = cx.executor().block_test(task);
    assert_eq!(n, Ok(2));
    assert_eq!(text_of(&a, cx), "fn bar() {}\nbar();\n");
    // b.rs was opened in a background tab, edited and left unsaved.
    let b = ws
        .read_with(cx, |ws, cx| {
            ws.all_file_views(cx)
                .into_iter()
                .find(|v| v.read(cx).path() == b_path.as_path())
        })
        .expect("b.rs tab");
    assert_eq!(text_of(&b, cx), "use bar;\n");
    b.read_with(cx, |v, cx| assert!(v.is_dirty(cx)));
    assert_eq!(std::fs::read_to_string(&b_path).unwrap(), "use foo;\n");
    // The active tab did not change.
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_path(cx), Some(fx.root.join("a.rs")))
    });
    // One undo step per file.
    cx.dispatch_action(Undo);
    cx.run_until_parked();
    assert_eq!(text_of(&a, cx), "fn foo() {}\nfoo();\n");
}

#[gpui::test]
fn stale_versioned_edit_is_rejected_atomically(cx: &mut TestAppContext) {
    let s = settings(false);
    init(cx, &s);
    let fx = fixture(&[("a.rs", "one\n"), ("b.rs", "two\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root, s);
    let a = open_file(&ws, fx.root.join("a.rs"), cx);
    let b = open_file(&ws, fx.root.join("b.rs"), cx);
    let edit = WorkspaceEdit {
        changes: vec![
            (fx.root.join("b.rs"), vec![edit(pt(0, 0)..pt(0, 3), "TWO")]),
            (fx.root.join("a.rs"), vec![edit(pt(0, 0)..pt(0, 3), "ONE")]),
        ],
        // The server computed a.rs at version 5; no server knows it.
        versions: vec![(fx.root.join("a.rs"), 5)],
    };
    let task = ws.update_in(cx, |ws, window, cx| {
        super::apply_workspace_edit(ws, edit, window, cx)
    });
    let result = cx.executor().block_test(task);
    assert!(result.is_err(), "{result:?}");
    assert_eq!(text_of(&a, cx), "one\n");
    assert_eq!(text_of(&b, cx), "two\n");
}

#[gpui::test]
fn diagnostics_map_through_edits_and_render(cx: &mut TestAppContext) {
    let s = settings(false);
    init(cx, &s);
    let fx = fixture(&[("a.rs", "let bad = 1;\nok\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root, s);
    let view = open_file(&ws, fx.root.join("a.rs"), cx);
    let path = fx.root.join("a.rs");
    let v0 = version_of(&view, cx);
    // Type before the problem; the server answers for the old text later.
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 0), cx))
    });
    cx.simulate_input("xx");
    cx.run_until_parked();
    let published = DocumentDiagnostics {
        path: path.clone(),
        server: "fake".into(),
        version: Some(1),
        tag: Some(v0),
        diagnostics: vec![lector_lsp::Diagnostic {
            range: pt(0, 4)..pt(0, 7),
            severity: DiagnosticSeverity::Error,
            message: "bad word".into(),
            source: Some("fake".into()),
            code: Some("F1".into()),
            tags: vec![],
            related: vec![],
        }],
    };
    let store = cx.update(|_, cx| DiagnosticsStore::global(cx));
    store.update(cx, |s, cx| s.published(&path, Some(published), cx));
    cx.run_until_parked();
    let entries = store.read_with(cx, |s, _| s.entries(&path).to_vec());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].range, 6..9, "mapped past the typed text");
    // Further edits keep moving it until the server republishes.
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 2), cx))
    });
    cx.simulate_input("y");
    cx.run_until_parked();
    let entries = store.read_with(cx, |s, _| s.entries(&path).to_vec());
    assert_eq!(entries[0].range, 7..10);
    assert_eq!(text_of(&view, cx)[7..10].to_string(), "bad");
    assert_eq!(store.read_with(cx, |s, _| s.counts()), (1, 0));
    // Squiggle and gutter icon in the tab.
    let marks = view.read_with(cx, |v, cx| v.code_view().read(cx).inline_marks().clone());
    assert_eq!(marks.underlines.len(), 1);
    assert_eq!(marks.underlines[0].row, 0);
    assert_eq!(marks.underlines[0].range, 7..10);
    assert!(marks.underlines[0].wavy);
    assert_eq!(marks.gutter_icons[0].row, 0);
    // Hover text and F8 navigation.
    let md = cx.update(|_, cx| super::diagnostics::hover_markdown(&path, 8, cx));
    assert_eq!(md.as_deref(), Some("**error** `fake(F1)`: bad word"));
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(1, 1), cx))
    });
    cx.dispatch_action(crate::actions::editor::NextDiagnostic);
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert_eq!(v.code_view().read(cx).cursor(), Pos::new(0, 7))
    });
    // Clearing.
    store.update(cx, |s, cx| s.published(&path, None, cx));
    assert!(store.read_with(cx, |s, _| s.entries(&path).is_empty()));
    let marks = view.read_with(cx, |v, cx| v.code_view().read(cx).inline_marks().clone());
    assert!(marks.underlines.is_empty());
}

fn have_python() -> bool {
    lector_lsp::which("python3").is_some()
}

fn wait_for_server(ws: &Entity<Workspace>, path: &Path, cx: &mut VisualTestContext) {
    let path = path.to_path_buf();
    let ws = ws.clone();
    wait_until(cx, |cx| {
        ws.read_with(cx, |ws, _| {
            ws.project()
                .lsp()
                .and_then(|l| l.capabilities(&path))
                .is_some()
        })
    });
}

#[gpui::test]
fn format_with_server_is_one_undo_step(cx: &mut TestAppContext) {
    if !have_python() {
        return;
    }
    let s = settings(true);
    init(cx, &s);
    let fx = fixture(&[("main.rs", "a  \nb\t\nc\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root, s);
    let view = open_file(&ws, fx.root.join("main.rs"), cx);
    wait_for_server(&ws, &fx.root.join("main.rs"), cx);
    let features = cx.update(|_, cx| super::features_for_path(&fx.root.join("main.rs"), cx));
    assert!(features.format && features.rename && features.code_actions);
    cx.dispatch_action(Format);
    wait_until(cx, |cx| text_of(&view, cx) == "a\nb\nc\n");
    cx.dispatch_action(Undo);
    cx.run_until_parked();
    assert_eq!(text_of(&view, cx), "a  \nb\t\nc\n");
    // Nothing was written.
    assert_eq!(
        std::fs::read_to_string(fx.root.join("main.rs")).unwrap(),
        "a  \nb\t\nc\n"
    );
    ws.read_with(cx, |ws, _| ws.project().lsp().cloned())
        .unwrap()
        .shutdown();
}

#[gpui::test]
fn code_action_quick_fix_from_server_diagnostics(cx: &mut TestAppContext) {
    if !have_python() {
        return;
    }
    let s = settings(true);
    init(cx, &s);
    let fx = fixture(&[("main.rs", "x bad y\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root, s);
    let path = fx.root.join("main.rs");
    let view = open_file(&ws, path.clone(), cx);
    wait_for_server(&ws, &path, cx);
    let store = cx.update(|_, cx| DiagnosticsStore::global(cx));
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.entries(&path).len() == 1)
    });
    assert_eq!(
        store.read_with(cx, |s, _| s.entries(&path)[0].range.clone()),
        2..5
    );
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 3), cx))
    });
    cx.dispatch_action(CodeActions);
    wait_until(cx, |cx| {
        view.read_with(cx, |v, cx| v.code_view().read(cx).overlay().is_some())
    });
    // The preferred quick fix is selected: Enter applies it.
    cx.dispatch_action(crate::actions::picker::Confirm);
    wait_until(cx, |cx| text_of(&view, cx) == "x good y\n");
    // The server republishes for the new text: no more problems.
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.entries(&path).is_empty())
    });
    ws.read_with(cx, |ws, _| ws.project().lsp().cloned())
        .unwrap()
        .shutdown();
}

#[gpui::test]
fn rename_across_open_and_unopened_files(cx: &mut TestAppContext) {
    if !have_python() {
        return;
    }
    let s = settings(true);
    init(cx, &s);
    let fx = fixture(&[
        ("main.rs", "fn foo() {}\nfoo();\n"),
        ("other.rs", "use foo;\n"),
    ]);
    let (ws, cx) = open_workspace(cx, &fx.root, s);
    let path = fx.root.join("main.rs");
    let view = open_file(&ws, path.clone(), cx);
    wait_for_server(&ws, &path, cx);
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(1, 1), cx))
    });
    cx.dispatch_action(Rename);
    wait_until(cx, |cx| {
        view.read_with(cx, |v, cx| v.code_view().read(cx).overlay().is_some())
    });
    // The placeholder is selected: typing replaces it.
    cx.simulate_input("bar");
    cx.dispatch_action(crate::actions::picker::Confirm);
    wait_until(cx, |cx| text_of(&view, cx) == "fn bar() {}\nbar();\n");
    let other = fx.root.join("other.rs");
    wait_until(cx, |cx| {
        ws.read_with(cx, |ws, cx| {
            ws.all_file_views(cx).into_iter().any(|v| {
                v.read(cx).path() == other.as_path()
                    && v.read(cx)
                        .model()
                        .read(cx)
                        .buffer()
                        .is_some_and(|b| b.read(cx).text().to_string() == "use bar;\n")
            })
        })
    });
    // Unsaved: the user reviews and saves.
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "use foo;\n");
    ws.read_with(cx, |ws, _| ws.project().lsp().cloned())
        .unwrap()
        .shutdown();
}
