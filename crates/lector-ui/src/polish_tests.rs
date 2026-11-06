//! GPUI integration tests for the editing polish pass: live Markdown preview,
//! files deleted on disk, Save As onto an open file, blame on dirty buffers
//! and find & replace.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, Entity, Focusable, TestAppContext, VisualTestContext};
use lector_theme::{Appearance, ThemeRegistry};

use crate::actions::editor::*;
use crate::editor::FileView;
use crate::theme::{ThemeMode, ThemeState};
use crate::views::markdown_preview::MarkdownPreview;
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
    let root = dunce::canonicalize(dir.path()).unwrap();
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

/// The Markdown preview open in any pane of `ws`.
fn preview_of(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<MarkdownPreview>> {
    ws.read_with(cx, |ws, cx| {
        ws.panes().into_iter().find_map(|pane| {
            pane.read(cx)
                .items()
                .find_map(|i| i.view().downcast::<MarkdownPreview>().ok())
        })
    })
}

#[gpui::test]
fn markdown_preview_shows_the_unsaved_buffer(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("doc.md", b"# Title\n\nfirst paragraph\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("doc.md"), cx);
    cx.dispatch_action(ToggleMarkdownPreview);
    cx.run_until_parked();
    let preview = preview_of(&ws, cx).expect("preview opened");
    preview.read_with(cx, |p, _| {
        assert_eq!(p.rendered_text(), ["Title", "first paragraph"]);
    });

    // Edits show up without saving, after the debounce.
    view.update(cx, |v, cx| {
        v.code_view().update(cx, |c, cx| {
            c.set_cursor(crate::code_view::Pos::new(2, 15), cx)
        })
    });
    cx.simulate_input(" and more");
    assert!(view.read_with(cx, |v, cx| v.is_dirty(cx)));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    preview.read_with(cx, |p, _| {
        assert_eq!(p.rendered_text(), ["Title", "first paragraph and more"]);
    });
    assert_eq!(
        std::fs::read_to_string(fx.root.join("doc.md")).unwrap(),
        "# Title\n\nfirst paragraph\n",
        "the file on disk is untouched"
    );

    // Undo brings the old text back.
    for _ in 0..20 {
        if !view.read_with(cx, |v, cx| v.is_dirty(cx)) {
            break;
        }
        cx.dispatch_action(Undo);
    }
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    preview.read_with(cx, |p, _| {
        assert_eq!(p.rendered_text(), ["Title", "first paragraph"]);
    });
    assert_eq!(view_text(&view, cx), "# Title\n\nfirst paragraph\n");
}

fn touch_later() {
    // File stamps compare mtime and length: make sure a rewrite differs.
    std::thread::sleep(std::time::Duration::from_millis(20));
}

#[gpui::test]
fn deleted_file_marks_the_tab_and_offers_to_recreate_it(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"hello\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    let deleted_tabs = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.read_with(cx, |ws, cx| {
            ws.active_pane()
                .read(cx)
                .items()
                .filter(|i| i.is_deleted(cx))
                .count()
        })
    };

    // Clean buffer: the tab stays, struck through; no banner.
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    assert_eq!(deleted_tabs(&ws, cx), 1);
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_pane().read(cx).item_count(), 1)
    });
    view.read_with(cx, |v, cx| assert!(!v.shows_deleted_banner(cx)));
    assert_eq!(view_text(&view, cx), "hello\n", "the text stays");

    // The file coming back (e.g. a branch switch) clears the mark and reloads.
    touch_later();
    std::fs::write(fx.root.join("a.txt"), "back\n").unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    assert_eq!(deleted_tabs(&ws, cx), 0);
    assert_eq!(view_text(&view, cx), "back\n");

    // Unsaved changes: banner; Save recreates the file.
    cx.simulate_input("X");
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    assert_eq!(deleted_tabs(&ws, cx), 1);
    view.read_with(cx, |v, cx| assert!(v.shows_deleted_banner(cx)));
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert!(
        !cx.has_pending_prompt(),
        "no conflict prompt for a missing file"
    );
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "backX\n"
    );
    assert_eq!(deleted_tabs(&ws, cx), 0);
    view.read_with(cx, |v, cx| {
        assert!(!v.shows_deleted_banner(cx));
        assert!(!v.is_dirty(cx));
    });

    // A clean tab of a deleted file: Save writes it again too.
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    assert_eq!(deleted_tabs(&ws, cx), 1);
    cx.dispatch_action(Save);
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "backX\n"
    );
    assert_eq!(deleted_tabs(&ws, cx), 0);

    // Close in the banner closes the tab after asking about the changes.
    cx.simulate_input("Y");
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    view.update(cx, |_, cx| {
        cx.emit(crate::editor::FileViewEvent::CloseRequested)
    });
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    ws.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_pane().read(cx).item_count(), 0)
    });
    assert!(!fx.root.join("a.txt").exists(), "nothing was written");
}

fn pane_files(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.read_with(cx, |ws, cx| ws.active_pane().read(cx).item_count())
}

#[gpui::test]
fn save_as_onto_an_open_file_asks_and_closes_its_clean_tab(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"alpha\n"), ("b.txt", b"beta\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let _b = open_file(&ws, fx.root.join("b.txt"), cx);
    let a = open_file(&ws, fx.root.join("a.txt"), cx);
    assert_eq!(pane_files(&ws, cx), 2);
    cx.simulate_input("X");

    let target = fx.root.join("b.txt");
    let answer = target.clone();
    cx.dispatch_action(SaveAs);
    cx.run_until_parked();
    cx.simulate_new_path_selection(move |_| Some(answer.clone()));
    cx.run_until_parked();

    // Cancel keeps everything.
    assert!(cx.has_pending_prompt(), "asks before replacing");
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(pane_files(&ws, cx), 2);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "beta\n");

    // Replace: the other tab closes, the file is overwritten, this tab follows.
    cx.dispatch_action(SaveAs);
    cx.run_until_parked();
    let answer = target.clone();
    cx.simulate_new_path_selection(move |_| Some(answer.clone()));
    cx.run_until_parked();
    cx.simulate_prompt_answer("Replace");
    cx.run_until_parked();
    assert_eq!(pane_files(&ws, cx), 1);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "Xalpha\n");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "alpha\n"
    );
    a.read_with(cx, |v, cx| {
        assert_eq!(v.path(), target.as_path());
        assert!(!v.is_dirty(cx));
    });
}

#[gpui::test]
fn save_as_onto_a_file_with_unsaved_changes_is_refused(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"alpha\n"), ("b.txt", b"beta\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let b = open_file(&ws, fx.root.join("b.txt"), cx);
    cx.simulate_input("B");
    let a = open_file(&ws, fx.root.join("a.txt"), cx);
    cx.simulate_input("A");

    let target = fx.root.join("b.txt");
    cx.dispatch_action(SaveAs);
    cx.run_until_parked();
    let answer = target.clone();
    cx.simulate_new_path_selection(move |_| Some(answer.clone()));
    cx.run_until_parked();
    let (message, _) = cx.pending_prompt().expect("told about the changes");
    assert!(message.contains("unsaved changes"), "{message}");
    cx.simulate_prompt_answer("Ok");
    cx.run_until_parked();

    assert_eq!(pane_files(&ws, cx), 2);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "beta\n");
    assert_eq!(view_text(&b, cx), "Bbeta\n", "their edits are kept");
    a.read_with(cx, |v, cx| {
        assert_eq!(v.path(), fx.root.join("a.txt").as_path());
        assert!(v.is_dirty(cx));
    });
}

fn find_bar_of(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<crate::views::find_bar::FindBar> {
    ws.read_with(cx, |ws, cx| ws.find_bar(cx))
        .expect("find bar is open")
}

/// Opens the bar with `query` typed in; leaves the focus in the find field.
fn open_find(
    ws: &Entity<Workspace>,
    query: &str,
    cx: &mut VisualTestContext,
) -> Entity<crate::views::find_bar::FindBar> {
    cx.dispatch_action(Find);
    cx.run_until_parked();
    cx.simulate_input(query);
    cx.run_until_parked();
    find_bar_of(ws, cx)
}

fn type_replacement(replacement: &str, cx: &mut VisualTestContext) {
    cx.dispatch_action(FindReplace);
    cx.run_until_parked();
    cx.simulate_input(replacement);
    cx.run_until_parked();
}

fn close_bar(cx: &mut VisualTestContext) {
    cx.dispatch_action(crate::actions::picker::Cancel);
    cx.run_until_parked();
}

#[gpui::test]
fn replace_all_is_one_undo_step(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"foo bar Foo\nfoo\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    let bar = open_find(&ws, "foo", cx);
    assert!(!bar.read_with(cx, |b, _| b.replace_open()), "find alone");
    type_replacement("qux", cx);
    assert!(bar.read_with(cx, |b, _| b.replace_open()));
    cx.dispatch_action(ReplaceAll);
    cx.run_until_parked();
    // Smart case: the lowercase query also matches "Foo".
    assert_eq!(view_text(&view, cx), "qux bar qux\nqux\n");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "foo bar Foo\nfoo\n",
        "nothing is saved"
    );
    close_bar(cx);
    cx.dispatch_action(Undo);
    assert_eq!(view_text(&view, cx), "foo bar Foo\nfoo\n", "one undo step");
    assert!(!view.read_with(cx, |v, cx| v.is_dirty(cx)));
    cx.dispatch_action(Redo);
    assert_eq!(view_text(&view, cx), "qux bar qux\nqux\n");
}

#[gpui::test]
fn regex_replace_expands_capture_groups(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"one two\nthree four\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    let bar = open_find(&ws, r"(\w+) (\w+)", cx);
    // Without regex mode the pattern is literal: nothing to replace.
    type_replacement("$2 $1", cx);
    cx.dispatch_action(ReplaceAll);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "one two\nthree four\n");

    bar.update(cx, |b, cx| b.toggle(2, cx));
    cx.run_until_parked();
    cx.dispatch_action(ReplaceAll);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "two one\nfour three\n");
}

#[gpui::test]
fn replace_next_walks_through_the_matches(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"a1 a2\na3\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    open_find(&ws, "a", cx);
    type_replacement("bb", cx);
    cx.dispatch_action(ReplaceNext);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "bb1 a2\na3\n");
    // The next match is selected, ready for the next replacement.
    view.read_with(cx, |v, cx| {
        assert_eq!(v.code_view().read(cx).selected_text(), "a");
    });
    cx.dispatch_action(ReplaceNext);
    cx.run_until_parked();
    cx.dispatch_action(ReplaceNext);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "bb1 bb2\nbb3\n");
    // Nothing left: the text stays, the bar says so.
    cx.dispatch_action(ReplaceNext);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "bb1 bb2\nbb3\n");
}

#[gpui::test]
fn find_again_toggles_the_replace_row(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"text\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let _view = open_file(&ws, fx.root.join("a.txt"), cx);
    let bar = open_find(&ws, "t", cx);
    let open = |cx: &mut VisualTestContext| bar.read_with(cx, |b, _| b.replace_open());
    assert!(!open(cx));
    cx.dispatch_action(Find);
    assert!(open(cx), "Find again shows replace");
    cx.dispatch_action(Find);
    assert!(!open(cx), "and hides it");
    cx.dispatch_action(FindReplace);
    assert!(open(cx));
    bar.update_in(cx, |b, window, cx| b.toggle_replace(window, cx));
    assert!(!open(cx), "the chevron toggles too");
}

#[gpui::test]
fn replace_is_disabled_in_read_only_tabs(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"foo foo\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    view.update(cx, |v, cx| {
        v.code_view().update(cx, |c, cx| c.set_read_only(true, cx))
    });
    cx.dispatch_action(FindReplace);
    cx.run_until_parked();
    let bar = find_bar_of(&ws, cx);
    assert!(!bar.read_with(cx, |b, cx| b.can_replace(cx)));
    assert!(
        !bar.read_with(cx, |b, _| b.replace_open()),
        "no replace row"
    );
    cx.simulate_input("foo");
    cx.dispatch_action(ReplaceAll);
    cx.dispatch_action(ReplaceNext);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "foo foo\n");
    assert!(!view.read_with(cx, |v, cx| v.is_dirty(cx)));
    bar.update_in(cx, |b, window, cx| b.toggle_replace(window, cx));
    assert!(!bar.read_with(cx, |b, _| b.replace_open()));
}

/// Regression: Tab moves between the find and replace fields (it used to do
/// nothing, so the replacement was typed into the query).
#[gpui::test]
fn tab_switches_between_find_and_replace_fields(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture(&[("a.txt", b"x = 1\n")]);
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_file(&ws, fx.root.join("a.txt"), cx);
    cx.dispatch_action(FindReplace);
    cx.run_until_parked();
    cx.simulate_input("x");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("y");
    cx.dispatch_action(ReplaceAll);
    cx.run_until_parked();
    assert_eq!(view_text(&view, cx), "y = 1\n");
    // Tab again goes back to the query.
    cx.simulate_keystrokes("tab");
    cx.simulate_input("z");
    let bar = find_bar_of(&ws, cx);
    bar.read_with(cx, |b, cx| assert_eq!(b.query_text(cx), "xz"));
}
