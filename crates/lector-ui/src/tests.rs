//! GPUI integration tests (TestAppContext) for the code view, picker and tree.

use gpui::{App, Focusable, SharedString, TestAppContext};
use lector_theme::{Appearance, ThemeRegistry};

use crate::actions::editor::*;
use crate::actions::picker as picker_actions;
use crate::actions::tree::{CollapseSelected, ExpandSelected};
use crate::code_view::{CodeView, Pos};
use crate::components::picker::Picker;
use crate::components::tree_view::{TreeDelegate, TreeIcon, TreeNode, TreeView};
use crate::theme::{ThemeMode, ThemeState};
use crate::views::command_palette::CommandPaletteDelegate;

fn init(cx: &mut TestAppContext) {
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(crate::code_view::EditorSettings::default());
    });
}

#[gpui::test]
fn code_view_keyboard_movement_and_copy(cx: &mut TestAppContext) {
    init(cx);
    let (view, cx) = cx.add_window_view(|_, cx| {
        CodeView::from_text("hello world\nsecond line\n\tthird", None, cx)
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx)));

    cx.dispatch_action(MoveDown);
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(1, 0)));
    cx.dispatch_action(SelectRight);
    cx.dispatch_action(SelectRight);
    cx.dispatch_action(SelectRight);
    view.read_with(cx, |v, _| {
        assert_eq!(v.selection().anchor, Pos::new(1, 0));
        assert_eq!(v.selection().head, Pos::new(1, 3));
        assert_eq!(v.selected_text(), "sec");
    });
    cx.dispatch_action(Copy);
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("sec")
    );

    // Left collapses to the selection start; word motion; line end.
    cx.dispatch_action(MoveLeft);
    view.read_with(cx, |v, _| {
        assert!(v.selection().is_empty() && v.cursor() == Pos::new(1, 0))
    });
    cx.dispatch_action(MoveWordRight);
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(1, 6)));
    cx.dispatch_action(MoveToLineEnd);
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(1, 11)));
    // Moving down keeps the goal column, clamped to the shorter line.
    cx.dispatch_action(MoveDown);
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(2, 6)));
    cx.dispatch_action(MoveUp);
    view.read_with(cx, |v, _| assert_eq!(v.cursor(), Pos::new(1, 11)));

    cx.dispatch_action(SelectAll);
    view.read_with(cx, |v, _| {
        assert_eq!(v.selected_text(), "hello world\nsecond line\n\tthird");
    });
    cx.dispatch_action(ClearSelection);
    view.read_with(cx, |v, _| assert!(v.selection().is_empty()));

    // Copy with an empty selection copies the line.
    cx.dispatch_action(ScrollToTop);
    cx.dispatch_action(Copy);
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("hello world\n")
    );
    // Tab-expanded column in the status bar.
    view.update(cx, |v, cx| v.set_cursor(Pos::new(2, 1), cx));
    view.read_with(cx, |v, _| assert_eq!(v.cursor_line_col(4), (3, 5)));
}

#[gpui::test]
fn code_view_mouse_selection_modes(cx: &mut TestAppContext) {
    init(cx);
    let (view, cx) = cx.add_window_view(|_, cx| {
        CodeView::from_text("let foo_bar = 1;\nnext line\nlast", None, cx)
    });
    view.update_in(cx, |v, window, cx| {
        // Double click selects a word; dragging extends by words.
        v.begin_selection(Pos::new(0, 6), 2, false, window, cx);
        assert_eq!(v.selected_text(), "foo_bar");
        v.extend_selection(Pos::new(1, 2), cx);
        assert_eq!(v.selected_text(), "foo_bar = 1;\nnext");
        v.end_selection();

        // Triple click selects the whole line including its newline.
        v.begin_selection(Pos::new(1, 3), 3, false, window, cx);
        assert_eq!(v.selected_text(), "next line\n");
        v.end_selection();

        // Click then shift-click extends.
        v.begin_selection(Pos::new(0, 4), 1, false, window, cx);
        v.end_selection();
        v.begin_selection(Pos::new(2, 2), 1, true, window, cx);
        v.end_selection();
        assert_eq!(v.selected_text(), "foo_bar = 1;\nnext line\nla");
    });
}

#[gpui::test]
fn code_view_folding_hides_rows(cx: &mut TestAppContext) {
    init(cx);
    let text = "fn a() {\n    b();\n    c();\n}\nfn d() {}\n";
    let (view, cx) = cx.add_window_view(|_, cx| CodeView::from_text(text, Some("x.rs"), cx));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx)));
    view.update(cx, |v, cx| v.set_folded(0, true, cx));
    cx.run_until_parked();
    // Layout rebuilds the display map; moving down from row 0 skips the fold.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.dispatch_action(MoveDown);
    view.read_with(cx, |v, _| {
        assert!(v.is_folded(0));
        assert_eq!(v.cursor().row, 3);
    });
    cx.dispatch_action(UnfoldAll);
    view.read_with(cx, |v, _| assert!(!v.is_folded(0)));
}

#[gpui::test]
fn command_palette_filters_actions(cx: &mut TestAppContext) {
    init(cx);
    let (picker, cx) = cx.add_window_view(|window, cx| {
        let delegate = CommandPaletteDelegate::new(None, cx);
        Picker::new(delegate, window, cx)
    });
    picker.update_in(cx, |p, window, cx| {
        p.set_query("toggle sidebar", window, cx)
    });
    picker.read_with(cx, |p, _| {
        let labels = p.delegate.match_labels();
        assert_eq!(
            labels.first().map(|s| s.as_ref()),
            Some("Workspace: Toggle Sidebar")
        );
        // Motion primitives are hidden from the palette.
        assert!(!labels.iter().any(|l| l.contains("Move Up")));
    });
    picker.update_in(cx, |p, window, cx| p.set_query("zzzzqqq", window, cx));
    picker.read_with(cx, |p, _| assert!(p.delegate.match_labels().is_empty()));
    picker.update_in(cx, |p, window, cx| p.set_query("", window, cx));
    let total = picker.read_with(cx, |p, _| p.delegate.match_labels().len());
    assert!(total > 40, "only {total} commands");

    // Keyboard navigation wraps.
    cx.update(|window, cx| window.focus(&picker.focus_handle(cx)));
    cx.dispatch_action(picker_actions::SelectPrev);
    picker.read_with(cx, |p, _| {
        use crate::components::picker::PickerDelegate;
        assert_eq!(p.delegate.selected_index(), total - 1);
    });
}

struct TestTree;

impl TreeDelegate for TestTree {
    type Id = String;

    fn children(&mut self, parent: Option<&String>, _cx: &mut App) -> Vec<TreeNode<String>> {
        let folder = |id: &str| {
            TreeNode::branch(
                id.to_string(),
                SharedString::from(id.to_string()),
                TreeIcon::Folder(id.to_string().into()),
            )
        };
        let file = |id: &str| {
            TreeNode::leaf(
                id.to_string(),
                SharedString::from(id.to_string()),
                TreeIcon::File(id.to_string().into()),
            )
        };
        match parent.map(String::as_str) {
            None => vec![folder("src"), file("README.md")],
            Some("src") => vec![folder("src/views"), file("src/lib.rs")],
            Some("src/views") => vec![file("src/views/a.rs")],
            _ => vec![],
        }
    }
}

#[gpui::test]
fn tree_view_expansion_and_keyboard(cx: &mut TestAppContext) {
    init(cx);
    let (tree, cx) = cx.add_window_view(|_, cx| TreeView::new("t", TestTree, cx));
    tree.read_with(cx, |t, _| assert_eq!(t.visible_count(), 2));
    cx.update(|window, cx| window.focus(&tree.focus_handle(cx)));
    cx.dispatch_action(picker_actions::SelectNext); // select "src"
    cx.dispatch_action(ExpandSelected);
    tree.read_with(cx, |t, _| {
        assert_eq!(t.visible_count(), 4);
        assert!(t.is_expanded(&"src".to_string()));
    });
    cx.dispatch_action(ExpandSelected); // moves to first child
    tree.read_with(cx, |t, _| {
        assert_eq!(t.selected_id().map(String::as_str), Some("src/views"))
    });
    cx.dispatch_action(picker_actions::Confirm); // toggles the folder open
    tree.read_with(cx, |t, _| {
        let labels: Vec<(usize, String)> = t
            .visible_labels()
            .into_iter()
            .map(|(d, l)| (d, l.to_string()))
            .collect();
        assert_eq!(
            labels,
            vec![
                (0, "src".into()),
                (1, "src/views".into()),
                (2, "src/views/a.rs".into()),
                (1, "src/lib.rs".into()),
                (0, "README.md".into()),
            ]
        );
    });
    cx.dispatch_action(CollapseSelected); // collapses src/views
    cx.dispatch_action(CollapseSelected); // moves to parent "src"
    tree.read_with(cx, |t, _| {
        assert_eq!(t.selected_id().map(String::as_str), Some("src"));
        assert_eq!(t.visible_count(), 4);
    });
    tree.update(cx, |t, cx| {
        t.reveal(
            &["src".into(), "src/views".into()],
            &"src/views/a.rs".into(),
            cx,
        )
    });
    tree.read_with(cx, |t, _| {
        assert_eq!(t.selected_id().map(String::as_str), Some("src/views/a.rs"))
    });
}

/// End-to-end: a workspace over a real project folder loads the file list,
/// opens a document with tree-sitter highlights and outline, and reveals a
/// position (all work happens on background executors).
#[gpui::test]
fn workspace_opens_documents_from_project(cx: &mut TestAppContext) {
    use crate::views::Workspace;

    init(cx);
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let applied = settings.clone();
    cx.update(|cx: &mut App| {
        cx.set_global(crate::settings::AppSettings {
            settings: std::sync::Arc::new(applied),
            warnings: Default::default(),
        })
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "struct Point {\n    x: i32,\n}\n\nfn main() {\n    let p = Point { x: 1 };\n}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("README.md"), "# Title\n\nHello *world*.\n").unwrap();
    std::fs::write(dir.path().join("data.bin"), [0u8, 1, 2, 0, 255]).unwrap();
    let project = lector_workspace::Project::open(dir.path(), settings).unwrap();
    let root = project.root().to_path_buf();

    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |ws, _| {
        let files: Vec<&str> = ws.file_list().paths.iter().map(|p| &**p).collect();
        assert_eq!(files, vec!["README.md", "data.bin", "src/main.rs"]);
    });

    let view = workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(root.join("src/main.rs"), Some(6), Some(9), window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        let doc = v.document().expect("loaded");
        assert_eq!(doc.language().map(|l| l.name.as_str()), Some("Rust"));
        assert!(doc.outline().iter().any(|i| i.name == "Point"));
        let code = v.code_view().read(cx);
        assert_eq!(code.cursor(), Pos::new(5, 8));
        assert_eq!(code.line_text(4).as_ref(), "fn main() {");
        assert_eq!(v.meta().unwrap().encoding.as_ref(), "UTF-8");
    });
    // Highlights arrive from the background for requested rows.
    let source = view.read_with(cx, |v, cx| v.code_view().read(cx).source().clone());
    let _ = source.highlights(0..7);
    cx.run_until_parked();
    assert!(source.highlights(0..7).iter().any(
        |s| matches!(s.style, crate::code_view::SpanStyle::Capture(c) if c.starts_with("keyword"))
    ));
    assert!(!source.sticky_rows(1).is_empty());

    // Binary files show a panel, not text.
    let bin = workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(root.join("data.bin"), None, None, window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    bin.read_with(cx, |v, _| assert!(v.meta().is_none()));
    workspace.read_with(cx, |ws, cx| {
        assert!(ws.active_code_view(cx).is_none());
        assert_eq!(ws.file_views(cx).len(), 2);
    });

    // GoBack returns to the previous location (recorded on the jump).
    cx.update(|window, cx| {
        let focus = bin.focus_handle(cx);
        window.focus(&focus);
    });
    cx.dispatch_action(crate::actions::workspace::GoBack);
    cx.run_until_parked();
    workspace.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_path(cx), Some(root.join("src/main.rs")));
        let cursor = ws.active_code_view(cx).unwrap().read(cx).cursor();
        assert_eq!(cursor, Pos::new(5, 8));
    });
    cx.dispatch_action(crate::actions::workspace::GoForward);
    cx.run_until_parked();
    workspace.read_with(cx, |ws, cx| {
        assert_eq!(ws.active_path(cx), Some(root.join("data.bin")));
    });
}
