//! GPUI integration tests for editing (v0.3): text input and IME, editing
//! actions, multiple carets, undo/redo, saving, dirty-state prompts and
//! external changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, AppContext, Entity, EntityInputHandler, Focusable, TestAppContext, VisualTestContext,
};
use lector_buffer::BufferOptions;
use lector_theme::{Appearance, ThemeRegistry};

use crate::actions::editor::*;
use crate::code_view::{CodeView, Pos, Selection};
use crate::editor::FileView;
use crate::theme::{ThemeMode, ThemeState};
use crate::views::Workspace;

fn init(cx: &mut TestAppContext, configure: impl FnOnce(&mut lector_config::Settings)) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    configure(&mut settings);
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(crate::code_view::EditorSettings::default());
        cx.set_global(crate::settings::AppSettings {
            settings: Arc::new(settings),
            warnings: Default::default(),
        });
    });
}

fn editable_view<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (Entity<CodeView>, &'a mut VisualTestContext) {
    let text = text.to_string();
    let options = BufferOptions {
        comment: lector_buffer::CommentTokens {
            line: Some("//".into()),
            block: None,
        },
        ..BufferOptions::default()
    };
    let (view, cx) =
        cx.add_window_view(move |_, cx| CodeView::editable(&text, Some("x.rs"), options, cx));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx)));
    cx.run_until_parked();
    (view, cx)
}

fn buffer_text(view: &Entity<CodeView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |v, cx| {
        v.buffer().expect("buffer").read(cx).text().to_string()
    })
}

#[gpui::test]
fn typing_ime_undo_redo(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let (view, cx) = editable_view(cx, "fn main() {}\n");
    view.update(cx, |v, cx| v.set_cursor(Pos::new(0, 11), cx));
    // Plain typing through the platform input handler.
    cx.simulate_input("let");
    assert_eq!(buffer_text(&view, cx), "fn main() {let}\n");
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(0, 14)));

    // IME composition: marked text is replaced, then committed.
    view.update_in(cx, |v, window, cx| {
        v.replace_and_mark_text_in_range(None, "ｋ", Some(1..1), window, cx);
        assert_eq!(v.marked_text_range(window, cx), Some(14..15));
        v.replace_and_mark_text_in_range(None, "か", Some(1..1), window, cx);
        assert_eq!(v.marked_text_range(window, cx), Some(14..15));
        v.replace_text_in_range(None, "漢", window, cx);
        assert_eq!(v.marked_text_range(window, cx), None);
    });
    assert_eq!(buffer_text(&view, cx), "fn main() {let漢}\n");
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(0, 17)));
    // The selection reported to the platform is in UTF-16.
    view.update_in(cx, |v, window, cx| {
        let sel = v.selected_text_range(false, window, cx).unwrap();
        assert_eq!(sel.range, 15..15);
        let mut adjusted = None;
        assert_eq!(
            v.text_for_range(11..15, &mut adjusted, window, cx)
                .as_deref(),
            Some("let漢")
        );
    });

    // The whole composition is one undo step; redo restores it.
    cx.dispatch_action(Undo);
    assert_eq!(buffer_text(&view, cx), "fn main() {let}\n");
    cx.dispatch_action(Redo);
    assert_eq!(buffer_text(&view, cx), "fn main() {let漢}\n");

    // Read-only views ignore input.
    view.update(cx, |v, cx| v.set_read_only(true, cx));
    cx.simulate_input("zz");
    cx.dispatch_action(Backspace);
    assert_eq!(buffer_text(&view, cx), "fn main() {let漢}\n");
    // ToggleReadOnly re-enables editing.
    cx.dispatch_action(ToggleReadOnly);
    cx.dispatch_action(Backspace);
    assert_eq!(buffer_text(&view, cx), "fn main() {let}\n");
}

#[gpui::test]
fn editing_actions_and_newline_indent(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let (view, cx) = editable_view(cx, "fn a() {\n    one();\n}\n");
    view.update(cx, |v, cx| v.set_cursor(Pos::new(1, 10), cx));
    cx.dispatch_action(Newline);
    cx.simulate_input("two();");
    assert_eq!(
        buffer_text(&view, cx),
        "fn a() {\n    one();\n    two();\n}\n"
    );
    cx.dispatch_action(DuplicateLine);
    cx.dispatch_action(MoveLineUp);
    cx.dispatch_action(ToggleComment);
    let text = buffer_text(&view, cx);
    assert!(text.contains("//"), "{text}");
    cx.dispatch_action(Undo);
    cx.dispatch_action(DeleteLine);
    assert_eq!(
        buffer_text(&view, cx),
        "fn a() {\n    one();\n    two();\n}\n"
    );
    // Cut with an empty selection cuts the line; paste puts it back.
    view.update(cx, |v, cx| v.set_cursor(Pos::new(1, 0), cx));
    cx.dispatch_action(Cut);
    assert_eq!(buffer_text(&view, cx), "fn a() {\n    two();\n}\n");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("    one();\n")
    );
    cx.dispatch_action(Paste);
    assert_eq!(
        buffer_text(&view, cx),
        "fn a() {\n    one();\n    two();\n}\n"
    );
    // The view's own source follows the buffer.
    view.read_with(cx, |v, _| {
        assert_eq!(v.line_text(2).as_ref(), "    two();");
    });
}

#[gpui::test]
fn multi_cursor_typing_and_paste_distribution(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let (view, cx) = editable_view(cx, "alpha\nbeta\ngamma\n");
    view.update(cx, |v, cx| v.set_cursor(Pos::new(0, 0), cx));
    cx.dispatch_action(AddCursorBelow);
    cx.dispatch_action(AddCursorBelow);
    view.read_with(cx, |v, _| assert_eq!(v.selection_count(), 3));
    cx.simulate_input("- ");
    assert_eq!(buffer_text(&view, cx), "- alpha\n- beta\n- gamma\n");

    // Paste distributes one line per caret.
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("1\n2\n3".into()));
    cx.dispatch_action(Paste);
    assert_eq!(buffer_text(&view, cx), "- 1alpha\n- 2beta\n- 3gamma\n");

    // Escape keeps only the primary (newest) caret.
    cx.dispatch_action(ClearSelection);
    view.read_with(cx, |v, _| {
        assert_eq!(v.selection_count(), 1);
        assert_eq!(v.cursor().row, 2);
    });

    // Select all occurrences of a word, then replace them all.
    let (view, cx) = editable_view(cx, "foo bar foo\nfoo\n");
    view.update(cx, |v, cx| v.set_cursor(Pos::new(0, 1), cx));
    cx.dispatch_action(SelectAllOccurrences);
    view.read_with(cx, |v, _| assert_eq!(v.selection_count(), 3));
    cx.simulate_input("x");
    assert_eq!(buffer_text(&view, cx), "x bar x\nx\n");

    // Alt-click adds a caret; typing goes to both.
    view.update_in(cx, |v, window, cx| {
        v.begin_selection(Pos::new(0, 1), 1, false, false, window, cx);
        v.end_selection();
        v.begin_selection(Pos::new(1, 1), 1, false, true, window, cx);
        v.end_selection();
        assert_eq!(v.selection_count(), 2);
    });
    cx.simulate_input("!");
    assert_eq!(buffer_text(&view, cx), "x! bar x\nx!\n");
    cx.dispatch_action(Undo);
    assert_eq!(buffer_text(&view, cx), "x bar x\nx\n");
}

#[gpui::test]
fn two_views_share_a_buffer(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let (a, cx) = editable_view(cx, "one\ntwo\n");
    let buffer = a.read_with(cx, |v, _| v.buffer().cloned().unwrap());
    let b = cx.new(|cx| {
        let mut v = CodeView::new(a.read(cx).source().clone(), cx);
        v.set_buffer(Some(buffer.clone()), true, cx);
        v.set_selection(Selection::cursor(Pos::new(1, 3)), cx);
        v
    });
    a.update(cx, |v, cx| v.set_cursor(Pos::new(0, 0), cx));
    cx.simulate_input("zero\n");
    assert_eq!(buffer_text(&a, cx), "zero\none\ntwo\n");
    // The other view's caret followed its text.
    b.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(2, 3)));
}

// Workspace-level tests.

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn fixture(files: &[(&str, &[u8])]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }
    let root = dir.path().canonicalize().unwrap();
    Fixture { _dir: dir, root }
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
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
        let focus = view.focus_handle(cx);
        window.focus(&focus);
    });
    cx.run_until_parked();
    view
}

fn view_text(view: &Entity<FileView>, cx: &mut VisualTestContext) -> String {
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

#[gpui::test]
fn save_round_trip_keeps_crlf_utf16_and_permissions(cx: &mut TestAppContext) {
    init(cx, |_| {});
    // UTF-16LE with BOM and CRLF line endings.
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "héllo\r\nwörld\r\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let fx = fixture(&[
        ("crlf.txt", b"one\r\ntwo\r\n"),
        ("wide.txt", &utf16),
        ("bom.rs", b"\xEF\xBB\xBFfn a() {}\n"),
    ]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            fx.root.join("crlf.txt"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
    }
    let (ws, cx) = open_workspace(cx, &fx.root);

    // CRLF: a new line typed with Enter is written as CRLF.
    let view = open_file(&ws, fx.root.join("crlf.txt"), cx);
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(1, 3), cx))
    });
    cx.dispatch_action(Newline);
    cx.simulate_input("three");
    view.read_with(cx, |v, cx| {
        assert!(v.is_dirty(cx));
        assert_eq!(v.meta(cx).unwrap().line_ending.as_ref(), "CRLF");
    });
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert_eq!(
        std::fs::read(fx.root.join("crlf.txt")).unwrap(),
        b"one\r\ntwo\r\nthree\r\n"
    );
    view.read_with(cx, |v, cx| assert!(!v.is_dirty(cx)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fx.root.join("crlf.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o751);
    }

    // UTF-16LE + BOM survive.
    let view = open_file(&ws, fx.root.join("wide.txt"), cx);
    view.read_with(cx, |v, cx| {
        assert_eq!(v.meta(cx).unwrap().encoding.as_ref(), "UTF-16LE");
    });
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 0), cx))
    });
    cx.simulate_input("¡");
    cx.dispatch_action(Save);
    cx.run_until_parked();
    let mut expected = vec![0xFF, 0xFE];
    for unit in "¡héllo\r\nwörld\r\n".encode_utf16() {
        expected.extend_from_slice(&unit.to_le_bytes());
    }
    assert_eq!(std::fs::read(fx.root.join("wide.txt")).unwrap(), expected);

    // UTF-8 BOM survives.
    let view = open_file(&ws, fx.root.join("bom.rs"), cx);
    view.read_with(cx, |v, cx| {
        assert_eq!(v.meta(cx).unwrap().encoding.as_ref(), "UTF-8 with BOM");
    });
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 8), cx))
    });
    cx.simulate_input(" ");
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert_eq!(
        std::fs::read(fx.root.join("bom.rs")).unwrap(),
        b"\xEF\xBB\xBFfn a() { }\n"
    );
}

#[gpui::test]
fn save_applies_trim_and_final_newline(cx: &mut TestAppContext) {
    init(cx, |s| {
        s.editor.trim_trailing_whitespace = true;
        s.editor.ensure_final_newline = true;
    });
    let fx = fixture(&[("a.txt", b"keep  \nend")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(1, 3), cx))
    });
    cx.simulate_input("!");
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert_eq!(
        std::fs::read(fx.root.join("a.txt")).unwrap(),
        b"keep\nend!\n"
    );
    // The buffer matches the disk and is clean.
    assert_eq!(view_text(&view, cx), "keep\nend!\n");
    view.read_with(cx, |v, cx| assert!(!v.is_dirty(cx)));
}

#[gpui::test]
fn before_save_hooks_edit_the_buffer(cx: &mut TestAppContext) {
    init(cx, |_| {});
    cx.update(|cx: &mut App| {
        crate::editor::register_before_save(cx, |model, cx| {
            let len = model.read(cx).live_document()?.text()?.len();
            Some(gpui::Task::ready(Ok(vec![lector_buffer::TextEdit {
                range: len..len,
                new_text: "// formatted\n".into(),
            }])))
        });
    });
    let fx = fixture(&[("a.rs", b"fn a() {}\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let _view = open_file(&ws, fx.root.join("a.rs"), cx);
    cx.simulate_input("x");
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.rs")).unwrap(),
        "xfn a() {}\n// formatted\n"
    );
}

#[gpui::test]
fn dirty_tab_close_prompt(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"hello\n"), ("b.txt", b"other\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    cx.simulate_input("X");
    view.read_with(cx, |v, cx| assert!(v.is_dirty(cx)));
    ws.read_with(cx, |ws, cx| {
        let pane = ws.active_pane().read(cx);
        assert!(pane.items().any(|i| i.is_dirty(cx)));
    });
    ws.read_with(cx, |ws, cx| assert_eq!(ws.title_dirty_marker(cx), "● "));

    // Cancel keeps the tab.
    cx.dispatch_action(crate::actions::workspace::CloseTab);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_pane().read(cx).item_count(), 1)
    });

    // Save writes and closes.
    cx.dispatch_action(crate::actions::workspace::CloseTab);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Save");
    cx.run_until_parked();
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_pane().read(cx).item_count(), 0)
    });
    assert_eq!(std::fs::read(fx.root.join("a.txt")).unwrap(), b"Xhello\n");

    // Don't Save discards (Close Others covers several files at once).
    let a = open_file(&ws, fx.root.join("a.txt"), cx);
    cx.simulate_input("Y");
    let _b = open_file(&ws, fx.root.join("b.txt"), cx);
    cx.simulate_input("Z");
    a.read_with(cx, |v, cx| assert!(v.is_dirty(cx)));
    ws.update_in(cx, |ws, window, cx| {
        ws.active_pane().update(cx, |p, cx| p.close_all(window, cx));
    });
    cx.run_until_parked();
    let (message, _) = cx.pending_prompt().expect("prompt");
    assert!(message.contains("2 files"), "{message}");
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_pane().read(cx).item_count(), 0)
    });
    assert_eq!(std::fs::read(fx.root.join("a.txt")).unwrap(), b"Xhello\n");
    assert_eq!(std::fs::read(fx.root.join("b.txt")).unwrap(), b"other\n");
    // Discarded documents are gone: reopening shows the disk text.
    let a = open_file(&ws, fx.root.join("a.txt"), cx);
    assert_eq!(view_text(&a, cx), "Xhello\n");
}

#[gpui::test]
fn external_changes_reload_or_raise_a_conflict(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"line one\nline two\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(1, 5), cx))
    });

    // Clean buffer: reloads in place, the caret stays on its line.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(fx.root.join("a.txt"), "line zero\nline one\nline two\n").unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "line zero\nline one\nline two\n");
    view.read_with(cx, |v, cx| {
        assert!(!v.is_dirty(cx));
        assert_eq!(v.code_view().read(cx).cursor(), Pos::new(2, 5));
    });

    // Our own save does not trigger a reload or a conflict.
    cx.simulate_input("!");
    cx.dispatch_action(Save);
    cx.run_until_parked();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |v, cx| assert!(v.model().read(cx).conflict().is_none()));

    // Dirty buffer: a conflict banner instead of a reload.
    cx.simulate_input("?");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(fx.root.join("a.txt"), "changed elsewhere\n").unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert!(v.model().read(cx).conflict().is_some());
        assert!(v.is_dirty(cx));
    });
    assert!(view_text(&view, cx).contains("!?"));

    // Saving now asks Overwrite / Reload / Cancel; Cancel writes nothing.
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "changed elsewhere\n"
    );

    // Keep Mine, then save: overwrites without asking.
    view.update(cx, |v, cx| v.model().update(cx, |m, cx| m.keep_mine(cx)));
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert!(std::fs::read_to_string(fx.root.join("a.txt"))
        .unwrap()
        .contains("!?"));

    // Reload (discard mine) after another external change.
    cx.simulate_input("#");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(fx.root.join("a.txt"), "disk wins\n").unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    view.update(cx, |v, cx| v.model().update(cx, |m, cx| m.revert(cx)));
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "disk wins\n");
    view.read_with(cx, |v, cx| {
        assert!(!v.is_dirty(cx));
        assert!(v.model().read(cx).conflict().is_none());
    });
}

#[gpui::test]
fn edits_keep_highlights_and_reparse(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("main.rs", b"fn main() {\n    let x = 1;\n}\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("main.rs"), cx);
    let keyword_at = |view: &Entity<FileView>, cx: &mut VisualTestContext, row: u32| {
        view.read_with(cx, |v, cx| {
            let source = v.code_view().read(cx).source().clone();
            source.highlights(row..row + 1).iter().any(|s| {
                matches!(s.style, crate::code_view::SpanStyle::Capture(c) if c.starts_with("keyword"))
            })
        })
    };
    let _ = keyword_at(&view, cx, 0);
    cx.run_until_parked();
    assert!(keyword_at(&view, cx, 1));
    // Insert a line above: before the reparse lands, the old highlights are
    // shown shifted; afterwards the fresh ones.
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 0), cx))
    });
    view.update(cx, |v, cx| {
        v.code_view().update(cx, |c, cx| {
            c.edit_buffer(cx, |b| b.insert("// hi\n"));
        })
    });
    assert!(
        keyword_at(&view, cx, 2),
        "stale highlights shift with the text"
    );
    cx.run_until_parked();
    let _ = keyword_at(&view, cx, 2);
    cx.run_until_parked();
    assert!(keyword_at(&view, cx, 2));
    view.read_with(cx, |v, cx| {
        let doc = v.live_document(cx).unwrap();
        assert!(doc.syntax().is_some(), "reparsed");
        assert!(doc.text().unwrap().to_string().starts_with("// hi\n"));
    });
}

/// Typing latency on a 10k-line Rust file (main-thread handling of one
/// keystroke: input → buffer edit → snapshot/source rebuild → layout).
/// Run with `cargo test -p lector-ui typing_latency -- --ignored --nocapture`.
#[gpui::test]
#[ignore]
fn typing_latency_10k_lines(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let mut text = String::new();
    for i in 0..2500 {
        text.push_str(&format!(
            "fn function_{i}(x: u32) -> u32 {{\n    let y = x * {i} + 1; // comment\n    y\n}}\n"
        ));
    }
    let fx = fixture(&[("big.rs", text.as_bytes())]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("big.rs"), cx);
    for wrap in [false, true] {
        view.update(cx, |v, cx| {
            v.code_view().update(cx, |c, cx| {
                c.set_soft_wrap(wrap, cx);
                c.set_cursor(Pos::new(5001, 12), cx)
            })
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
        cx.run_until_parked();
        let mut input_us = Vec::new();
        let mut frame_us = Vec::new();
        for ch in "let value = compute(1, 2);".chars() {
            let started = std::time::Instant::now();
            view.update(cx, |v, cx| {
                v.code_view().update(cx, |c, cx| {
                    let s = ch.to_string();
                    c.edit_buffer(cx, move |b| b.insert(&s));
                })
            });
            input_us.push(started.elapsed().as_micros());
            let started = std::time::Instant::now();
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear();
            });
            frame_us.push(started.elapsed().as_micros());
            cx.run_until_parked();
        }
        input_us.sort();
        frame_us.sort();
        let p = |v: &[u128], q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!(
            "typing latency (10k lines, soft wrap {wrap}): edit+sync p50 {} µs p95 {} µs max {} µs; frame p50 {} µs p95 {} µs max {} µs",
            p(&input_us, 0.5),
            p(&input_us, 0.95),
            input_us.last().unwrap(),
            p(&frame_us, 0.5),
            p(&frame_us, 0.95),
            frame_us.last().unwrap()
        );
    }
}

/// Regression: Open Recent / Open Folder replace the window's workspace; with unsaved
/// edits they must ask first (Cancel keeps everything) instead of silently dropping them.
#[gpui::test]
fn opening_another_folder_in_the_window_asks_about_unsaved_changes(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"hello\n")]);
    let other = fixture(&[("b.txt", b"other\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    cx.simulate_input("X");
    ws.update_in(cx, |ws, window, cx| {
        ws.open_folder_in_window(other.root.clone(), None, window, cx)
    });
    cx.run_until_parked();
    assert!(
        cx.has_pending_prompt(),
        "asks before dropping unsaved edits"
    );
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    view.read_with(cx, |v, cx| assert!(v.is_dirty(cx), "edits kept"));
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_pane().read(cx).item_count(), 1)
    });
    assert_eq!(std::fs::read(fx.root.join("a.txt")).unwrap(), b"hello\n");

    // Save All writes, then the folder opens.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_folder_in_window(other.root.clone(), None, window, cx)
    });
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Save");
    cx.run_until_parked();
    assert_eq!(std::fs::read(fx.root.join("a.txt")).unwrap(), b"Xhello\n");
}

/// Regression: consecutive keystrokes in a code view form one undo step (the view
/// used to re-install its unchanged carets before every edit, which ended the
/// typing run, so Undo removed one character at a time).
#[gpui::test]
fn typing_in_a_code_view_is_one_undo_step(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let (view, cx) = editable_view(cx, "fn a() {}\n");
    view.update(cx, |v, cx| v.set_cursor(Pos::new(0, 9), cx));
    for c in ["/", "/", " ", "h", "i"] {
        cx.simulate_input(c);
    }
    assert_eq!(buffer_text(&view, cx), "fn a() {}// hi\n");
    cx.dispatch_action(Undo);
    assert_eq!(buffer_text(&view, cx), "fn a() {}\n");
    // Moving the caret still starts a new step.
    cx.dispatch_action(Redo);
    cx.simulate_input("x");
    view.update(cx, |v, cx| v.set_cursor(Pos::new(0, 0), cx));
    cx.simulate_input("y");
    cx.dispatch_action(Undo);
    assert_eq!(buffer_text(&view, cx), "fn a() {}// hix\n");
}

/// v0.4: the tab is a view of the shared store document. Edits an agent (or a
/// command, or another window) makes through the store reach the open tab
/// with carets mapped, typing continues after them, and a lock set from
/// outside locks the tab.
#[gpui::test]
fn store_edits_reach_open_tabs(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"hello\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    let handle = view.read_with(cx, |v, cx| v.model().read(cx).handle().cloned().unwrap());
    view.update(cx, |v, cx| {
        v.code_view()
            .update(cx, |c, cx| c.set_cursor(Pos::new(0, 5), cx))
    });
    // The store hands out the same document for the path.
    let store = cx.update(|_, cx| crate::editor::document_store(cx));
    assert!(store
        .get(&fx.root.join("a.txt"))
        .is_some_and(|h| h == handle));
    handle
        .apply_edits(vec![lector_buffer::TextEdit {
            range: 0..0,
            new_text: "agent: ".into(),
        }])
        .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert!(v.is_dirty(cx));
        let text = v.live_document(cx).unwrap().text().unwrap().to_string();
        assert_eq!(text, "agent: hello\n");
        assert_eq!(
            v.code_view().read(cx).cursor(),
            Pos::new(0, 12),
            "caret mapped"
        );
    });
    cx.simulate_input("!");
    assert_eq!(handle.text().unwrap().to_string(), "agent: hello!\n");
    // Saving from the store (an agent's `document.save`) cleans the tab.
    handle
        .save(lector_app::documents::SaveRequest::default())
        .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |v, cx| assert!(!v.is_dirty(cx)));
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "agent: hello!\n"
    );
    handle.set_read_only(true);
    cx.run_until_parked();
    view.read_with(cx, |v, cx| assert!(v.is_read_only(cx)));
    cx.simulate_input("x");
    assert_eq!(handle.text().unwrap().to_string(), "agent: hello!\n");
}
