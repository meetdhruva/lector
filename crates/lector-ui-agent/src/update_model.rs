//! The update check as the UI shows it (v0.7): scheduling, the persisted state around a check,
//! and the mapping from [`UpdateStatus`] to the words and buttons of the dialog and the status
//! bar. UI-free and unit-tested; `update_ui.rs` only draws what this module says.
//!
//! Policy (see "Updates and signing (v0.7)" in `docs/ARCHITECTURE.md`): notify only. Nothing is
//! downloaded or run by Lector; "Download" opens the asset URL in the browser. "Verified" is
//! said only when the checksum file's signature verified; otherwise the dialog says "signature
//! not available for this build".

use std::path::Path;

use lector_config::{should_check_now, UpdateChannel, UpdateCheck};
use lector_forge::releases::{
    check_for_update_with, compare_versions, Channel, CheckFailure, ReleaseAsset, UpdateAction,
    UpdateOptions, UpdateState, UpdateStatus,
};

/// Whether an automatic check should run now (`now` and `last_checked` in Unix seconds).
pub fn automatic_check_due(setting: UpdateCheck, last_checked: Option<i64>, now: i64) -> bool {
    should_check_now(last_checked, now, setting)
}

/// The channel of the forge crate for the `[updates] channel` setting.
pub fn forge_channel(channel: UpdateChannel) -> Channel {
    match channel {
        UpdateChannel::Stable => Channel::Stable,
        UpdateChannel::Preview => Channel::Preview,
    }
}

/// Runs a check with the persisted state at `state_path` (blocking: call off the UI thread) and
/// saves the new state. `opts` is [`UpdateOptions::default`] in production.
pub fn run_check(
    current_version: &str,
    channel: Channel,
    opts: &UpdateOptions,
    state_path: &Path,
) -> UpdateStatus {
    let mut state = UpdateState::load(state_path);
    let before = state.clone();
    let status = check_for_update_with(current_version, channel, opts, &mut state);
    if state != before {
        if let Err(e) = state.save(state_path) {
            tracing::warn!("update state not saved: {e}");
        }
    }
    status
}

/// Whether an automatic check is due according to the persisted state at `state_path`.
pub fn automatic_check_due_in(setting: UpdateCheck, state_path: &Path, now: i64) -> bool {
    automatic_check_due(setting, UpdateState::load(state_path).last_checked, now)
}

/// Whether `status` is a still-useful "available" answer for the running `current_version`.
pub fn is_newer_available(status: &UpdateStatus, current_version: &str) -> bool {
    match status {
        UpdateStatus::Available { version, .. } => {
            compare_versions(version, current_version) == Some(std::cmp::Ordering::Greater)
        }
        _ => false,
    }
}

/// What a dialog button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateButtonAction {
    /// Open this URL in the browser (the user installs the update themselves).
    OpenUrl(String),
    /// Copy this text (an update command or a checksum).
    Copy(String),
}

/// One button of the update dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateButton {
    /// Label.
    pub label: String,
    /// Effect.
    pub action: UpdateButtonAction,
    /// The primary (accented) button.
    pub primary: bool,
}

/// How sure we are that the release is genuine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// The checksum file's signature verified against the project key.
    Verified,
    /// No signature could be checked for this build.
    NotAvailable,
}

/// Everything the dialog shows for one check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateDialogModel {
    /// Dialog title.
    pub title: String,
    /// The first line under the title.
    pub headline: String,
    /// More lines (version, date, verification, hints), in order.
    pub details: Vec<String>,
    /// A copyable command or checksum shown in a monospace box, with its label.
    pub copyable: Option<(String, String)>,
    /// Buttons, primary first.
    pub buttons: Vec<UpdateButton>,
    /// Whether an update is available (the status bar item shows).
    pub available: bool,
    /// Verification state of an available update.
    pub verification: Option<Verification>,
}

/// "2026-11-13" for Unix seconds.
pub fn format_date(secs: i64) -> String {
    let secs = u64::try_from(secs).unwrap_or(0);
    lector_app::health::rfc3339(secs)
        .split('T')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A short, human reason a check failed.
pub fn failure_text(reason: &CheckFailure) -> String {
    match reason {
        CheckFailure::RateLimited { retry_at: Some(t) } => format!(
            "GitHub is limiting requests right now; it works again after {}.",
            format_date(*t)
        ),
        CheckFailure::RateLimited { retry_at: None } => {
            "GitHub is limiting requests right now; try again later.".into()
        }
        CheckFailure::Network(why) => format!("The network request failed ({why})."),
        CheckFailure::Http(code) => format!("GitHub answered with HTTP {code}."),
        CheckFailure::BadResponse(why) => {
            format!("GitHub's answer could not be understood ({why}).")
        }
        CheckFailure::InvalidVersion(v) => format!("This build's version ({v}) is not comparable."),
    }
}

/// The wording of the verification state. "Verified" appears only for [`Verification::Verified`].
pub fn verification_text(v: Verification) -> &'static str {
    match v {
        Verification::Verified => {
            "Verified: the release checksums are signed with the Lector project key."
        }
        Verification::NotAvailable => "Signature not available for this build.",
    }
}

/// Maps a check result to the dialog. `manual`: the user asked (Help, Check for Updates...), so
/// "up to date" and failures are shown as answers; automatic checks only ever surface an
/// available update.
pub fn dialog_model(status: &UpdateStatus, current_version: &str) -> UpdateDialogModel {
    match status {
        UpdateStatus::UpToDate => UpdateDialogModel {
            title: "Check for Updates".into(),
            headline: format!("Lector {current_version} is up to date."),
            details: vec!["You have the newest version of this channel.".into()],
            copyable: None,
            buttons: Vec::new(),
            available: false,
            verification: None,
        },
        UpdateStatus::CouldNotCheck { reason } => UpdateDialogModel {
            title: "Check for Updates".into(),
            headline: "Could not check for updates.".into(),
            details: vec![
                failure_text(reason),
                "Nothing was sent except the request for the release list.".into(),
            ],
            copyable: None,
            buttons: Vec::new(),
            available: false,
            verification: None,
        },
        UpdateStatus::Available {
            version,
            notes_url,
            published_at,
            prerelease,
            verified,
            action,
            hint,
            ..
        } => {
            let verification = if *verified {
                Verification::Verified
            } else {
                Verification::NotAvailable
            };
            let mut details = vec![format!(
                "Version {version}{}, you have {current_version}.",
                if *prerelease { " (pre-release)" } else { "" }
            )];
            if let Some(t) = published_at {
                details.push(format!("Published {}.", format_date(*t)));
            }
            details.push(verification_text(verification).into());
            let mut buttons = Vec::new();
            let mut copyable = None;
            match action {
                UpdateAction::Download { asset } => {
                    buttons.push(UpdateButton {
                        label: format!("Download {}", asset.name),
                        action: UpdateButtonAction::OpenUrl(asset.url.clone()),
                        primary: true,
                    });
                    details.push(
                        "Lector opens the download in your browser; it never downloads or installs anything itself."
                            .into(),
                    );
                    if let Some(sum) = sha256_of(asset) {
                        copyable = Some(("Expected SHA-256".into(), sum));
                    }
                }
                UpdateAction::PackageManager { command } => {
                    details.push("Update with your package manager:".into());
                    copyable = Some(("Command".into(), command.clone()));
                    buttons.push(UpdateButton {
                        label: "Copy command".into(),
                        action: UpdateButtonAction::Copy(command.clone()),
                        primary: true,
                    });
                }
                UpdateAction::ReleasePage => {
                    buttons.push(UpdateButton {
                        label: "Open release page".into(),
                        action: UpdateButtonAction::OpenUrl(notes_url.clone()),
                        primary: true,
                    });
                }
            }
            if let Some(h) = hint {
                details.push(h.clone());
            }
            if !matches!(action, UpdateAction::ReleasePage) {
                buttons.push(UpdateButton {
                    label: "Release notes".into(),
                    action: UpdateButtonAction::OpenUrl(notes_url.clone()),
                    primary: false,
                });
            }
            if let Some((_, sum)) = &copyable {
                if matches!(action, UpdateAction::Download { .. }) {
                    buttons.push(UpdateButton {
                        label: "Copy SHA-256".into(),
                        action: UpdateButtonAction::Copy(sum.clone()),
                        primary: false,
                    });
                }
            }
            UpdateDialogModel {
                title: "Update available".into(),
                headline: format!("Lector {version} is available."),
                details,
                copyable,
                buttons,
                available: true,
                verification: Some(verification),
            }
        }
    }
}

fn sha256_of(asset: &ReleaseAsset) -> Option<String> {
    asset.sha256.clone().filter(|s| !s.is_empty())
}

/// The status bar label of an available update ("Update 0.7.1"), `None` otherwise.
pub fn status_bar_label(status: &UpdateStatus) -> Option<String> {
    match status {
        UpdateStatus::Available { version, .. } => Some(format!("Update {version}")),
        _ => None,
    }
}

/// The one-time first-run question's text.
pub const FIRST_RUN_TITLE: &str = "Check for updates automatically?";
/// The body of the first-run question.
pub const FIRST_RUN_BODY: &str = "Lector would ask GitHub once a day whether a new version exists. Nothing about you or your code is sent.";

/// The line shown in the toast after the toggle command.
pub fn toggle_text(now_on: bool) -> &'static str {
    if now_on {
        "Automatic update checks are on (once a day)."
    } else {
        "Automatic update checks are off."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_forge::releases::InstallKind;

    fn asset(sha: Option<&str>) -> ReleaseAsset {
        ReleaseAsset {
            name: "lector-0.7.1-linux-x86_64.tar.gz".into(),
            url: "https://example.test/lector.tar.gz".into(),
            size: 10,
            sha256: sha.map(String::from),
        }
    }

    fn available(verified: bool, action: UpdateAction) -> UpdateStatus {
        UpdateStatus::Available {
            version: "0.7.1".into(),
            notes_url: "https://example.test/notes".into(),
            published_at: Some(1_700_000_000),
            prerelease: false,
            assets: vec![],
            verified,
            install_kind: InstallKind::Tarball,
            action,
            hint: None,
        }
    }

    #[test]
    fn scheduling_follows_setting_and_last_check() {
        let now = 1_800_000_000;
        assert!(!automatic_check_due(UpdateCheck::Off, None, now));
        assert!(automatic_check_due(UpdateCheck::On, None, now));
        assert!(!automatic_check_due(UpdateCheck::On, Some(now - 3600), now));
        assert!(automatic_check_due(
            UpdateCheck::On,
            Some(now - 25 * 3600),
            now
        ));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("update-state.json");
        assert!(automatic_check_due_in(UpdateCheck::On, &path, now));
        let state = UpdateState {
            last_checked: Some(now - 60),
            ..UpdateState::default()
        };
        state.save(&path).expect("save");
        assert!(!automatic_check_due_in(UpdateCheck::On, &path, now));
    }

    #[test]
    fn up_to_date_and_failures_are_answers() {
        let m = dialog_model(&UpdateStatus::UpToDate, "0.7.0");
        assert_eq!(m.headline, "Lector 0.7.0 is up to date.");
        assert!(!m.available && m.buttons.is_empty() && m.verification.is_none());
        let m = dialog_model(
            &UpdateStatus::CouldNotCheck {
                reason: CheckFailure::Network("dns".into()),
            },
            "0.7.0",
        );
        assert_eq!(m.headline, "Could not check for updates.");
        assert!(m.details[0].contains("dns"));
        assert!(!m.available);
        let m = dialog_model(
            &UpdateStatus::CouldNotCheck {
                reason: CheckFailure::RateLimited { retry_at: None },
            },
            "0.7.0",
        );
        assert!(m.details[0].contains("limiting"));
    }

    #[test]
    fn download_opens_the_browser_and_shows_the_checksum() {
        let sum = "ab".repeat(32);
        let m = dialog_model(
            &available(
                true,
                UpdateAction::Download {
                    asset: asset(Some(&sum)),
                },
            ),
            "0.7.0",
        );
        assert!(m.available);
        assert_eq!(m.headline, "Lector 0.7.1 is available.");
        assert!(m.details.iter().any(|d| d == "Published 2023-11-14."));
        assert_eq!(m.copyable, Some(("Expected SHA-256".into(), sum.clone())));
        assert_eq!(
            m.buttons[0].action,
            UpdateButtonAction::OpenUrl("https://example.test/lector.tar.gz".into())
        );
        assert!(m.buttons[0].primary);
        assert!(m
            .buttons
            .iter()
            .any(|b| b.action == UpdateButtonAction::Copy(sum.clone())));
        assert!(m
            .buttons
            .iter()
            .any(|b| b.action == UpdateButtonAction::OpenUrl("https://example.test/notes".into())));
    }

    #[test]
    fn verified_wording_only_when_verified() {
        let verified = dialog_model(&available(true, UpdateAction::ReleasePage), "0.7.0");
        let unverified = dialog_model(&available(false, UpdateAction::ReleasePage), "0.7.0");
        assert_eq!(verified.verification, Some(Verification::Verified));
        assert!(verified.details.iter().any(|d| d.starts_with("Verified:")));
        assert_eq!(unverified.verification, Some(Verification::NotAvailable));
        assert!(unverified
            .details
            .iter()
            .any(|d| d == "Signature not available for this build."));
        let all: String = unverified.details.join(" ").to_lowercase();
        assert!(!all.contains("verified"), "{all}");
        assert!(!unverified.headline.to_lowercase().contains("verified"));
        // An unverified Download does not claim anything either.
        let dl = dialog_model(
            &available(false, UpdateAction::Download { asset: asset(None) }),
            "0.7.0",
        );
        assert!(!dl.details.join(" ").to_lowercase().contains("verified"));
        assert!(dl.copyable.is_none());
    }

    #[test]
    fn package_manager_shows_a_copyable_command() {
        let m = dialog_model(
            &available(
                false,
                UpdateAction::PackageManager {
                    command: "brew upgrade --cask lector".into(),
                },
            ),
            "0.7.0",
        );
        assert_eq!(
            m.copyable,
            Some(("Command".into(), "brew upgrade --cask lector".into()))
        );
        assert_eq!(
            m.buttons[0].action,
            UpdateButtonAction::Copy("brew upgrade --cask lector".into())
        );
    }

    #[test]
    fn release_page_opens_the_notes_url() {
        let m = dialog_model(&available(false, UpdateAction::ReleasePage), "0.7.0");
        assert_eq!(m.buttons.len(), 1);
        assert_eq!(
            m.buttons[0].action,
            UpdateButtonAction::OpenUrl("https://example.test/notes".into())
        );
    }

    #[test]
    fn status_bar_and_staleness() {
        let s = available(false, UpdateAction::ReleasePage);
        assert_eq!(status_bar_label(&s).as_deref(), Some("Update 0.7.1"));
        assert_eq!(status_bar_label(&UpdateStatus::UpToDate), None);
        assert!(is_newer_available(&s, "0.7.0"));
        assert!(!is_newer_available(&s, "0.7.1"));
        assert!(!is_newer_available(&s, "0.8.0"));
        assert!(!is_newer_available(&UpdateStatus::UpToDate, "0.7.0"));
    }

    #[test]
    fn dates_and_channels() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(forge_channel(UpdateChannel::Preview), Channel::Preview);
        assert_eq!(forge_channel(UpdateChannel::Stable), Channel::Stable);
        assert!(toggle_text(true).contains("on"));
        assert!(toggle_text(false).contains("off"));
    }
}
