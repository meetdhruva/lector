//! The Activity log and "Undo last operation" in a workspace opened on a folder that is not a
//! git repository (ADR 0011): reached from their actions, they list and undo an agent turn's
//! journal entry, while the git views stay repository-only.

use std::path::Path;
use std::time::{Duration, SystemTime};

use gpui::{Focusable as _, SharedString, TestAppContext};
use lector_app::commands::agent_backup::AgentBackups;
use lector_app::commands::Source;
use lector_app::journal::{ActivityEntry, Journal};
use lector_app::Project;
use lector_app::{AppCore, CoreOptions};
use lector_ui::actions::workspace::{ShowActivity, UndoLastOperation};
use lector_ui::views::Workspace;

use crate::activity::ActivityView;

/// An agent turn in `root` as the agent session journals it: `todo.txt` rewritten.
fn journal_a_turn(core: &AppCore, project: &Project, root: &Path) -> u64 {
    let backups = AgentBackups::new(core, project);
    let mut ledger = backups.begin_turn("Before agent turn 1: groceries");
    backups
        .before_write(Some(&mut ledger), "todo.txt", 12)
        .expect("backed up");
    std::fs::write(root.join("todo.txt"), "milk\neggs\n").expect("write");
    let backup = backups
        .finish_turn(&ledger, "Agent turn 1: groceries")
        .expect("turn backed up");
    Journal::of(core)
        .expect("journal")
        .append(ActivityEntry {
            id: 0,
            time: SystemTime::now(),
            command: "agent.turn".into(),
            summary: "Fake turn 1: groceries (1 file)".into(),
            repo: Some(backups.workdir().to_path_buf()),
            paths: vec![root.join("todo.txt")],
            undo: backup.undo,
            undone: false,
            args: serde_json::json!({}),
            source: Source::Agent,
            undo_of: None,
            undone_by: None,
        })
        .expect("journaled")
}

#[gpui::test]
fn activity_log_and_undo_last_work_in_a_folder_without_git(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(dir.path())
        .expect("canonical")
        .join("notes");
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("todo.txt"), "milk\n").expect("write");
    let data = tempfile::tempdir().expect("tempdir");

    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let core = AppCore::open(CoreOptions {
        data_dir: Some(data.path().to_path_buf()),
        ..CoreOptions::for_tests()
    });
    let project = Project::open(&root, settings.clone()).expect("project");
    assert!(project.repo().is_none());
    let id = journal_a_turn(&core, &project, &root);
    cx.update(|cx| {
        lector_ui::ThemeState::init(
            cx,
            lector_theme::ThemeRegistry::new(),
            lector_ui::ThemeMode::Dark,
            lector_theme::Appearance::Dark,
        );
        cx.set_global(lector_ui::code_view::EditorSettings::default());
        cx.set_global(lector_ui::settings::AppSettings {
            settings: std::sync::Arc::new(settings.clone()),
            warnings: Default::default(),
        });
        cx.set_global(lector_ui::commands_bridge::CoreGlobal(core.clone()));
        crate::init(cx);
    });
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| crate::store_for(ws.entity_id(), cx))
            .is_none(),
        "the git views stay repository-only"
    );

    // `workspace::ShowActivity` opens the log, which lists the folder's turn.
    cx.update(|window, cx| window.focus(&ws.focus_handle(cx)));
    cx.run_until_parked();
    cx.dispatch_action(ShowActivity);
    cx.run_until_parked();
    let view = ws
        .read_with(cx, |ws, cx| {
            let pane = ws.active_pane().read(cx);
            let ix = pane.index_for_key(&SharedString::from("activity"), cx)?;
            pane.items().nth(ix)?.view().downcast::<ActivityView>().ok()
        })
        .expect("the Activity log tab opened from its action");
    let mut listed = false;
    for _ in 0..100 {
        cx.run_until_parked();
        listed = view.read_with(cx, |v, _| v.entry_ids().contains(&id));
        if listed {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(listed, "the turn's entry is listed");

    // `workspace::UndoLastOperation` undoes the turn: the file is back.
    cx.update(|window, cx| window.focus(&ws.focus_handle(cx)));
    cx.run_until_parked();
    cx.dispatch_action(UndoLastOperation);
    for _ in 0..100 {
        cx.run_until_parked();
        if std::fs::read_to_string(root.join("todo.txt"))
            .ok()
            .as_deref()
            == Some("milk\n")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(root.join("todo.txt")).expect("read"),
        "milk\n"
    );
    let journal = Journal::of(&core).expect("journal");
    assert!(journal.get(id).expect("entry").undone);
    let toast = ws.read_with(cx, |ws, _| ws.action_toast_message());
    assert!(
        toast.is_some_and(|t| t.contains("groceries")),
        "the undo offers Redo"
    );
    assert!(!root.join(".git").exists());
}
