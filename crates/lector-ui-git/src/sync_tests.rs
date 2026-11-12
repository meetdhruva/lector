//! GPUI tests of the sync UI: the controller against a local bare remote (push → rejected →
//! pull → push through the same code path the buttons use), the dirty-tree switch flow,
//! force-with-lease gating, and the credential dialogs (host-key default, masking, queue).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Once;
use std::time::Duration;

use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
use lector_app::{AppCore, CoreOptions};
use lector_git::SyncErrorKind;
use lector_process::askpass::{classify_prompt, PromptReply, PromptRequest};
use lector_ui::views::Workspace;
use lector_workspace::Project;

use crate::askpass_ui;
use crate::confirm::ConfirmDialog;
use crate::sync::SyncController;
use crate::sync_dialogs::{HostKeyChoice, NoticeDialog};
use crate::sync_model::{NextStep, PromptDialogKind, SyncAction};

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    });
}

fn git(dir: &Path, args: &[&str]) -> String {
    hermetic();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A bare remote and two clones (`alice` opened in Lector, `bob` driven by plain git).
struct Fixture {
    _tmp: tempfile::TempDir,
    data: tempfile::TempDir,
    remote: PathBuf,
    alice: PathBuf,
    bob: PathBuf,
}

fn fixture() -> Fixture {
    hermetic();
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let remote = base.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let clone = |name: &str| {
        let dir = base.join(name);
        git(
            &base,
            &[
                "clone",
                "-q",
                &remote.to_string_lossy(),
                &dir.to_string_lossy(),
            ],
        );
        for (k, v) in [
            ("user.name", name),
            ("user.email", "sync@example.com"),
            ("core.autocrlf", "false"),
            ("gc.auto", "0"),
        ] {
            git(&dir, &["config", k, v]);
        }
        dir
    };
    let alice = clone("alice");
    git(&alice, &["checkout", "-q", "-B", "main"]);
    std::fs::write(alice.join("a.txt"), "one\n").unwrap();
    git(&alice, &["add", "-A"]);
    git(&alice, &["commit", "-q", "-m", "first"]);
    git(&alice, &["push", "-q", "-u", "origin", "main"]);
    let bob = clone("bob");
    Fixture {
        _tmp: tmp,
        data: tempfile::tempdir().unwrap(),
        remote,
        alice,
        bob,
    }
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
    data: &Path,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.executor().allow_parking();
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let core = AppCore::open(CoreOptions {
        data_dir: Some(data.to_path_buf()),
        ..CoreOptions::for_tests()
    });
    let runtime = data.join("run");
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
        cx.set_global(lector_ui::commands_bridge::CoreGlobal(core));
        askpass_ui::set_runtime_dir(runtime, cx);
        crate::init(cx);
    });
    let project = Project::open(root, settings).unwrap();
    let (ws, vcx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    vcx.run_until_parked();
    (ws, vcx)
}

fn controller(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<SyncController> {
    cx.update(|_, cx| {
        crate::store_for(ws.entity_id(), cx)
            .and_then(|s| s.read(cx).sync.clone())
            .expect("a sync controller for the workspace")
    })
}

/// Runs the GPUI loop until the controller is idle (the commands run on scheduler threads).
fn wait_idle(c: &Entity<SyncController>, cx: &mut VisualTestContext) {
    for _ in 0..300 {
        cx.run_until_parked();
        if c.read_with(cx, |c, _| c.running().is_none()) {
            cx.run_until_parked();
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("the sync operation did not finish");
}

/// Waits until the status (upstream, counts) has been loaded.
fn wait_status(c: &Entity<SyncController>, cx: &mut VisualTestContext) {
    for _ in 0..200 {
        cx.run_until_parked();
        if c.read_with(cx, |c, _| c.status().loaded) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("status never loaded");
}

fn active_notice(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<NoticeDialog>> {
    ws.read_with(cx, |ws, cx| {
        ws.modal_layer().read(cx).active_modal::<NoticeDialog>()
    })
}

/// commit in bob → push; commit in alice → Push is rejected (dialog: "Pull First") → Pull
/// First fails as diverged (dialog: "Pull (Rebase)") → rebase pull succeeds and offers Push
/// again → Push succeeds. Every step through the controller methods the buttons call.
#[gpui::test]
fn push_rejected_pull_then_push_against_a_bare_remote(cx: &mut TestAppContext) {
    let fx = fixture();
    // Bob moves the remote.
    std::fs::write(fx.bob.join("b.txt"), "bob\n").unwrap();
    git(&fx.bob, &["add", "-A"]);
    git(&fx.bob, &["commit", "-q", "-m", "bob's change"]);
    git(&fx.bob, &["push", "-q"]);
    // Alice commits locally without fetching.
    std::fs::write(fx.alice.join("c.txt"), "alice\n").unwrap();
    git(&fx.alice, &["add", "-A"]);
    git(&fx.alice, &["commit", "-q", "-m", "alice's change"]);

    let (ws, cx) = open_workspace(cx, &fx.alice, fx.data.path());
    let c = controller(&ws, cx);
    wait_status(&c, cx);
    let view = c.read_with(cx, |c, _| crate::sync_model::control_view(c.status()));
    assert_eq!(view.state_word, "push", "alice is ahead before fetching");
    assert_eq!(view.counts, "↑1");

    // Push: rejected, the dialog offers "Pull First".
    cx.update(|window, cx| c.update(cx, |c, cx| c.run_action(SyncAction::Push, window, cx)));
    wait_idle(&c, cx);
    let failure = c
        .read_with(cx, |c, _| c.last_failure().cloned())
        .expect("push failed");
    assert_eq!(failure.kind, Some(SyncErrorKind::Rejected));
    assert_eq!(
        failure.steps,
        vec![NextStep::PullFirst, NextStep::PullRebase]
    );
    let notice = active_notice(&ws, cx).expect("failure dialog shown");
    let labels = notice.read_with(cx, |d, _| d.button_labels());
    assert_eq!(labels[0].as_ref(), "Pull First");
    assert_eq!(
        notice.read_with(cx, |d, _| d.title().to_string()),
        "Push failed"
    );

    // Pull First: alice has her own commit, so a fast-forward is impossible.
    cx.update(|window, cx| c.update(cx, |c, cx| c.run_step(NextStep::PullFirst, window, cx)));
    wait_idle(&c, cx);
    let failure = c
        .read_with(cx, |c, _| c.last_failure().cloned())
        .expect("pull failed");
    assert_eq!(failure.kind, Some(SyncErrorKind::Diverged));
    assert_eq!(failure.steps, vec![NextStep::PullRebase]);

    // Pull (Rebase): succeeds, offers to push again.
    cx.update(|window, cx| c.update(cx, |c, cx| c.run_step(NextStep::PullRebase, window, cx)));
    wait_idle(&c, cx);
    assert!(c.read_with(cx, |c, _| c.last_failure().is_none()));
    let msg = c
        .read_with(cx, |c, _| c.last_message().cloned())
        .unwrap_or_default();
    assert!(msg.contains("now push again"), "{msg}");
    assert!(fx.alice.join("b.txt").exists(), "bob's commit pulled");
    let toast = ws.read_with(cx, |ws, _| ws.action_toast_message());
    assert!(toast.is_some_and(|t| t.contains("now push again")));

    // Push again: succeeds.
    cx.update(|window, cx| c.update(cx, |c, cx| c.run_action(SyncAction::Push, window, cx)));
    wait_idle(&c, cx);
    assert!(c.read_with(cx, |c, _| c.last_failure().is_none()));
    let msg = c
        .read_with(cx, |c, _| c.last_message().cloned())
        .unwrap_or_default();
    assert_eq!(msg.as_ref(), "Pushed main → origin/main");
    let remote_log = git(&fx.remote, &["log", "--format=%s", "main"]);
    assert!(remote_log.contains("alice's change") && remote_log.contains("bob's change"));
    assert!(
        !git(&fx.alice, &["log", "--merges", "--format=%H"]).contains(char::is_alphanumeric),
        "no merge commit"
    );

    // The control shows the state after a refresh: up to date.
    wait_status(&c, cx);
    for _ in 0..100 {
        cx.run_until_parked();
        if c.read_with(cx, |c, _| {
            crate::sync_model::control_view(c.status()).state_word
        }) == "up to date"
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        c.read_with(cx, |c, _| crate::sync_model::control_view(c.status())
            .state_word),
        "up to date"
    );
}

/// Switching with local changes that would be overwritten: the dialog lists the files and
/// offers "Stash Changes and Switch"; nothing is stashed until that is chosen.
#[gpui::test]
fn dirty_tree_switch_offers_stash_and_switch(cx: &mut TestAppContext) {
    let fx = fixture();
    git(&fx.alice, &["checkout", "-q", "-b", "other"]);
    std::fs::write(fx.alice.join("a.txt"), "other branch\n").unwrap();
    git(&fx.alice, &["commit", "-q", "-am", "other"]);
    git(&fx.alice, &["checkout", "-q", "main"]);
    std::fs::write(fx.alice.join("a.txt"), "my local edit\n").unwrap();

    let (ws, cx) = open_workspace(cx, &fx.alice, fx.data.path());
    let c = controller(&ws, cx);
    wait_status(&c, cx);
    cx.update(|window, cx| c.update(cx, |c, cx| c.switch_branch("other".into(), window, cx)));
    wait_idle(&c, cx);
    let failure = c
        .read_with(cx, |c, _| c.last_failure().cloned())
        .expect("switch refused");
    assert_eq!(failure.kind, Some(SyncErrorKind::LocalChanges));
    assert!(
        failure.paths.iter().any(|p| p == "a.txt"),
        "{:?}",
        failure.paths
    );
    assert_eq!(
        failure.steps,
        vec![NextStep::StashAndSwitch("other".into())]
    );
    let notice = active_notice(&ws, cx).expect("dialog");
    assert_eq!(
        notice.read_with(cx, |d, _| d.button_labels())[0].as_ref(),
        "Stash Changes and Switch"
    );
    assert_eq!(
        git(&fx.alice, &["stash", "list"]),
        "",
        "never stashed silently"
    );
    assert_eq!(git(&fx.alice, &["branch", "--show-current"]).trim(), "main");

    cx.update(|window, cx| {
        c.update(cx, |c, cx| {
            c.run_step(NextStep::StashAndSwitch("other".into()), window, cx)
        })
    });
    wait_idle(&c, cx);
    assert!(c.read_with(cx, |c, _| c.last_failure().is_none()));
    assert_eq!(
        git(&fx.alice, &["branch", "--show-current"]).trim(),
        "other"
    );
    assert!(git(&fx.alice, &["stash", "list"]).contains("before switching to other"));
    assert_eq!(
        std::fs::read_to_string(fx.alice.join("a.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        "other branch\n"
    );
}

/// Force push is never run without the confirmation dialog.
#[gpui::test]
fn force_push_with_lease_waits_for_confirmation(cx: &mut TestAppContext) {
    let fx = fixture();
    std::fs::write(fx.alice.join("x.txt"), "x\n").unwrap();
    git(&fx.alice, &["add", "-A"]);
    git(&fx.alice, &["commit", "-q", "-m", "x"]);
    let before = git(&fx.remote, &["rev-parse", "main"]);
    let (ws, cx) = open_workspace(cx, &fx.alice, fx.data.path());
    let c = controller(&ws, cx);
    wait_status(&c, cx);
    cx.update(|window, cx| {
        c.update(cx, |c, cx| {
            c.run_action(SyncAction::ForcePushWithLease, window, cx)
        })
    });
    cx.run_until_parked();
    assert!(
        c.read_with(cx, |c, _| c.running().is_none()),
        "nothing started"
    );
    let dialog = ws.read_with(cx, |ws, cx| {
        ws.modal_layer().read(cx).active_modal::<ConfirmDialog>()
    });
    assert!(dialog.is_some(), "confirmation shown");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        git(&fx.remote, &["rev-parse", "main"]),
        before,
        "remote untouched"
    );
    // Escape cancels: still nothing pushed.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(c.read_with(cx, |c, _| c.running().is_none()));
    assert_eq!(git(&fx.remote, &["rev-parse", "main"]), before);
}

fn request(text: &str) -> PromptRequest {
    PromptRequest {
        text: text.into(),
        kind: classify_prompt(text),
    }
}

const HOST_PROMPT: &str = "The authenticity of host 'git.example (10.0.0.1)' can't be established.\nED25519 key fingerprint is SHA256:abcDEF123.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";

/// Credential dialogs: the host-key dialog defaults to Cancel (Enter = Cancel) and its
/// trust answer is the literal `yes`; passwords are masked; prompts queue one at a time;
/// cancelling the operation cancels waiting prompts.
#[gpui::test]
fn credential_prompts_default_safely_mask_and_queue(cx: &mut TestAppContext) {
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.alice, fx.data.path());
    let _ = controller(&ws, cx);

    // Host key: Enter answers Cancel.
    let (tx, rx) = mpsc::channel();
    cx.update(|_, cx| askpass_ui::show_prompt(request(HOST_PROMPT), tx, cx));
    cx.run_until_parked();
    let dialog = cx
        .update(|_, cx| askpass_ui::showing(cx))
        .expect("host key dialog");
    dialog.read_with(cx, |d, _| {
        assert_eq!(d.spec().kind, PromptDialogKind::HostKey);
        assert_eq!(d.spec().fingerprint.as_deref(), Some("SHA256:abcDEF123"));
        assert_eq!(d.spec().key_type.as_deref(), Some("ED25519"));
        assert_eq!(d.choice(), HostKeyChoice::Cancel, "Cancel is the default");
    });
    cx.update(|window, cx| window.focus(&dialog.focus_handle(cx)));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).ok(),
        Some(PromptReply::Cancel)
    );

    // Arrow (or Tab) to "Trust and continue", Enter: the answer is exactly "yes".
    let (tx, rx) = mpsc::channel();
    cx.update(|_, cx| askpass_ui::show_prompt(request(HOST_PROMPT), tx, cx));
    cx.run_until_parked();
    let dialog = cx.update(|_, cx| askpass_ui::showing(cx)).expect("dialog");
    cx.update(|window, cx| window.focus(&dialog.focus_handle(cx)));
    cx.simulate_keystrokes("right");
    assert_eq!(
        dialog.read_with(cx, |d, _| d.choice()),
        HostKeyChoice::Trust
    );
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(PromptReply::Answer(s)) => assert_eq!(s.expose(), "yes"),
        other => panic!("expected yes, got {other:?}"),
    }

    // Password: masked; two prompts queue; the typed answer goes back; cancel_pending cancels
    // the one still waiting.
    let (tx1, rx1) = mpsc::channel();
    let (tx2, rx2) = mpsc::channel();
    cx.update(|_, cx| {
        askpass_ui::show_prompt(
            request("Password for 'https://ann:tok@git.example': "),
            tx1,
            cx,
        );
        askpass_ui::show_prompt(request("Username for 'https://git.example': "), tx2, cx);
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| askpass_ui::queued(cx)), 1);
    let dialog = cx
        .update(|_, cx| askpass_ui::showing(cx))
        .expect("password dialog");
    let input = dialog.read_with(cx, |d, _| {
        assert_eq!(d.spec().kind, PromptDialogKind::Password);
        assert!(!d.spec().url.clone().unwrap_or_default().contains("tok"));
        d.input().clone()
    });
    assert!(input.read_with(cx, |i, _| i.is_masked()));
    cx.update(|window, cx| window.focus(&dialog.focus_handle(cx)));
    cx.simulate_input("s3cret");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    match rx1.recv_timeout(Duration::from_secs(2)) {
        Ok(PromptReply::Answer(s)) => assert_eq!(s.expose(), "s3cret"),
        other => panic!("expected the password, got {other:?}"),
    }
    assert_eq!(
        input.read_with(cx, |i, _| i.text().to_string()),
        "",
        "field cleared"
    );
    let next = cx
        .update(|_, cx| askpass_ui::showing(cx))
        .expect("queued prompt shown next");
    assert_eq!(
        next.read_with(cx, |d, _| d.spec().kind),
        PromptDialogKind::Username
    );
    cx.update(|_, cx| askpass_ui::cancel_pending(cx));
    cx.run_until_parked();
    assert_eq!(
        rx2.recv_timeout(Duration::from_secs(2)).ok(),
        Some(PromptReply::Cancel)
    );

    // A dialog closed without an answer (outside click, window closing) answers Cancel.
    let (tx, rx) = mpsc::channel();
    cx.update(|_, cx| askpass_ui::show_prompt(request("Enter passphrase for key '/k': "), tx, cx));
    cx.run_until_parked();
    assert!(cx.update(|_, cx| askpass_ui::showing(cx)).is_some());
    cx.update(|window, cx| {
        let layer = ws.read(cx).modal_layer().clone();
        layer.update(cx, |l, cx| l.hide_modal(window, cx));
    });
    cx.run_until_parked();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).ok(),
        Some(PromptReply::Cancel)
    );
}
