//! Lector's own small UI state for the update and health notices (v0.7): which one-time
//! questions were already asked and answered. Stored in Lector's state store
//! (`lector_app::store`, concern `notices`), never in the user's settings or project (see
//! "Write policy" in `docs/ARCHITECTURE.md`). The `notices.json` of 0.7.0 and 0.7.1 is
//! imported into the store once.
//!
//! UI-free: plain data, and reads and atomic read-modify-writes of the store's value.

use lector_app::store::Single;
use lector_config::UpdateCheck;
use lector_forge::releases::UpdateStatus;
use serde::{Deserialize, Serialize};

/// What the notices remember between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notices {
    /// The first-run question "Check for updates automatically?" was shown (and answered or
    /// dismissed). Never asked again.
    pub updates_asked: bool,
    /// The user's choice about automatic checks, made in the question or with the toggle
    /// command. `None`: no choice, the `[updates] check` setting decides (default off).
    pub auto_update_check: Option<bool>,
    /// Id of the last abnormal session the user was told about.
    pub acknowledged_crash: Option<String>,
    /// The last "update available" answer, so the status bar item can show without a request
    /// (automatic checks run at most once a day). Dropped once the running version caught up.
    pub pending_update: Option<UpdateStatus>,
}

/// The store's handle for the notices.
pub type NoticesStore = Single<Notices>;

impl Notices {
    /// Reads the stored value; a missing or unreadable value is the empty state.
    pub fn load(store: &NoticesStore) -> Notices {
        match store.get() {
            Ok(n) => n.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("notices: {e}");
                Notices::default()
            }
        }
    }

    /// Applies `f` to the stored value in one transaction (so two windows or processes
    /// answering at once do not undo each other) and returns its result with the new value.
    pub fn update<R>(
        store: &NoticesStore,
        f: impl FnOnce(&mut Notices) -> R,
    ) -> lector_app::store::StoreResult<(R, Notices)> {
        store.update(|v| {
            let n = v.get_or_insert_with(Notices::default);
            let r = f(n);
            (r, n.clone())
        })
    }

    /// Whether automatic checks are on: the user's choice here wins over `[updates] check`.
    pub fn effective_check(&self, setting: UpdateCheck) -> UpdateCheck {
        match self.auto_update_check {
            Some(true) => UpdateCheck::On,
            Some(false) => UpdateCheck::Off,
            None => setting,
        }
    }

    /// Whether to show the one-time "Check for updates automatically?" question now: never asked,
    /// and checks are not already on through the settings file.
    pub fn should_ask_about_updates(&self, setting: UpdateCheck) -> bool {
        !self.updates_asked && self.effective_check(setting) == UpdateCheck::Off
    }

    /// Records the answer to the one-time question (`turn_on`: "Turn on" / "Not now").
    pub fn answer_update_question(&mut self, turn_on: bool) {
        self.updates_asked = true;
        self.auto_update_check = if turn_on {
            Some(true)
        } else {
            self.auto_update_check
        };
    }

    /// Records that the question was seen and dismissed without an answer (closing the window,
    /// Escape): asked once is asked once.
    pub fn dismiss_update_question(&mut self) {
        self.updates_asked = true;
    }

    /// Flips the automatic checks (the toggle command) and returns the new state.
    pub fn toggle_auto_check(&mut self, setting: UpdateCheck) -> bool {
        let now_on = self.effective_check(setting) == UpdateCheck::Off;
        self.auto_update_check = Some(now_on);
        self.updates_asked = true;
        now_on
    }

    /// Whether the abnormal session `id` still needs its one-time note.
    pub fn needs_crash_note(&self, id: &str) -> bool {
        self.acknowledged_crash.as_deref() != Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asked_once_and_default_stays_off() {
        let mut n = Notices::default();
        assert_eq!(n.effective_check(UpdateCheck::Off), UpdateCheck::Off);
        assert!(n.should_ask_about_updates(UpdateCheck::Off));
        // "Not now": dismissed, never asked again, still off.
        n.answer_update_question(false);
        assert!(!n.should_ask_about_updates(UpdateCheck::Off));
        assert_eq!(n.effective_check(UpdateCheck::Off), UpdateCheck::Off);
        // Closing the question without answering counts as asked too.
        let mut n = Notices::default();
        n.dismiss_update_question();
        assert!(!n.should_ask_about_updates(UpdateCheck::Off));
        assert_eq!(n.auto_update_check, None);
    }

    #[test]
    fn turn_on_enables_checks_and_settings_on_skips_the_question() {
        let mut n = Notices::default();
        n.answer_update_question(true);
        assert_eq!(n.effective_check(UpdateCheck::Off), UpdateCheck::On);
        // A user who already turned checks on in settings.toml is not asked.
        assert!(!Notices::default().should_ask_about_updates(UpdateCheck::On));
    }

    #[test]
    fn the_toggle_flips_the_effective_state() {
        let mut n = Notices::default();
        assert!(n.toggle_auto_check(UpdateCheck::Off));
        assert_eq!(n.effective_check(UpdateCheck::Off), UpdateCheck::On);
        assert!(!n.toggle_auto_check(UpdateCheck::Off));
        assert_eq!(n.effective_check(UpdateCheck::On), UpdateCheck::Off);
        // From settings = on with no choice, the toggle turns it off.
        let mut n = Notices::default();
        assert!(!n.toggle_auto_check(UpdateCheck::On));
    }

    #[test]
    fn the_crash_note_is_shown_once_per_session() {
        let mut n = Notices::default();
        assert!(n.needs_crash_note("s1"));
        n.acknowledged_crash = Some("s1".into());
        assert!(!n.needs_crash_note("s1"));
        assert!(n.needs_crash_note("s2"));
    }

    #[test]
    fn the_store_round_trips_and_updates_are_atomic() {
        let store = lector_app::store::Store::in_memory();
        let notices: NoticesStore = store.notices();
        assert_eq!(Notices::load(&notices), Notices::default());
        let ((), n) = Notices::update(&notices, |n| {
            n.answer_update_question(true);
            n.acknowledged_crash = Some("s9".into());
        })
        .expect("update");
        assert_eq!(Notices::load(&notices), n);
        let (on, _) =
            Notices::update(&notices, |n| n.toggle_auto_check(UpdateCheck::Off)).expect("toggle");
        assert!(!on);
        assert_eq!(Notices::load(&notices).auto_update_check, Some(false));
        assert_eq!(
            Notices::load(&notices).acknowledged_crash.as_deref(),
            Some("s9")
        );
    }

    /// The `notices.json` 0.7.1 wrote (fixture produced by the 0.7.1 code) reads back from the
    /// store after the import, field for field.
    #[test]
    fn the_notices_of_0_7_1_are_imported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../lector-app/tests/fixtures/state-0.7.1/notices.json");
        std::fs::copy(&fixture, dir.path().join("notices.json")).expect("copy");
        let store = lector_app::store::Store::in_memory();
        store.import_legacy(dir.path());
        let n = Notices::load(&store.notices());
        assert!(n.updates_asked);
        assert_eq!(n.auto_update_check, Some(true));
        assert_eq!(n.acknowledged_crash.as_deref(), Some("1795240000-4242"));
        assert!(matches!(
            n.pending_update,
            Some(UpdateStatus::Available { ref version, verified: true, .. }) if version == "0.8.0"
        ));
    }
}
