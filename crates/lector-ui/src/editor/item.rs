//! The pane item of a file tab ([`FileItem`]) and the close-window guard
//! that asks about unsaved changes ([`confirm_discard_or_save`]).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    prelude::*, AnyView, App, Entity, FocusHandle, Focusable, PromptLevel, SharedString, Task,
    WeakEntity, Window,
};

use super::{FileView, Reveal};
use crate::code_view::CodeView;
use crate::views::pane::{PaneItem, TabIcon};

/// The pane item for a [`FileView`].
pub struct FileItem {
    /// The view.
    pub view: Entity<FileView>,
    code_view: Entity<CodeView>,
    is_text: Rc<Cell<bool>>,
}

impl FileItem {
    /// Item for `view`.
    pub fn new(view: Entity<FileView>, cx: &App) -> Self {
        let v = view.read(cx);
        FileItem {
            code_view: v.code_view().clone(),
            is_text: v.is_text_flag(),
            view,
        }
    }

    /// Weak handle to the view.
    pub fn weak_view(&self) -> WeakEntity<FileView> {
        self.view.downgrade()
    }
}

impl PaneItem for FileItem {
    fn title(&self, cx: &App) -> SharedString {
        self.view
            .read(cx)
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .into()
    }

    fn icon(&self, cx: &App) -> TabIcon {
        TabIcon::File(self.title(cx))
    }

    fn path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.view.read(cx).path().to_path_buf())
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn code_view(&self) -> Option<Entity<CodeView>> {
        self.is_text.get().then(|| self.code_view.clone())
    }

    fn file_view(&self) -> Option<Entity<FileView>> {
        Some(self.view.clone())
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.view.read(cx).is_dirty(cx)
    }

    fn is_deleted(&self, cx: &App) -> bool {
        self.view.read(cx).model().read(cx).is_deleted()
    }

    fn is_read_only(&self, cx: &App) -> bool {
        let v = self.view.read(cx);
        self.is_text.get() && v.model().read(cx).is_editable() && v.is_read_only(cx)
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let project = self.view.read(cx).project().clone();
        let path = self.view.read(cx).path().to_path_buf();
        let selection = self.code_view.read(cx).selection();
        let lock = self.view.read(cx).read_only_lock().cloned();
        let view = cx.new(|cx| {
            let mut v = FileView::new(path, project, cx);
            v.set_read_only_lock(lock, cx);
            v.reveal(
                Reveal {
                    row: selection.head.row,
                    cols: selection.head.col..selection.head.col,
                    center: true,
                },
                cx,
            );
            v
        });
        Some(Box::new(FileItem::new(view, cx)))
    }
}

/// Closes the window after asking about unsaved changes in `workspace`
/// (Save All / Don't Save / Cancel). Resolves to `true` when it may close.
pub fn confirm_discard_or_save(
    dirty: Vec<Entity<FileView>>,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<bool>> {
    if dirty.is_empty() {
        return Task::ready(Ok(true));
    }
    let names: Vec<String> = dirty.iter().map(|v| v.read(cx).file_name()).collect();
    let (message, detail) = if names.len() == 1 {
        (
            format!(
                "Do you want to save the changes you made to “{}”?",
                names[0]
            ),
            "Your changes will be lost if you don't save them.".to_string(),
        )
    } else {
        (
            format!("Do you want to save the changes to {} files?", names.len()),
            format!(
                "{}\n\nYour changes will be lost if you don't save them.",
                names.join("\n")
            ),
        )
    };
    let save_label = if names.len() == 1 { "Save" } else { "Save All" };
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        Some(&detail),
        &[save_label, "Don't Save", "Cancel"],
        cx,
    );
    window.spawn(cx, async move |cx| match answer.await {
        Ok(0) => {
            for view in dirty {
                let task = view.update_in(cx, |v, window, cx| v.save(window, cx))?;
                if !task.await? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Ok(1) => Ok(true),
        _ => Ok(false),
    })
}
