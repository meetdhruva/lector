//! Update-check policy (v0.7): the `[updates]` settings and the schedule helper.
//!
//! The check itself lives in `lector_forge::releases`. Nothing here talks to the network; the
//! last-check time and the `ETag` are stored by the caller next to Lector's other state
//! (`paths::data_dir()/update-state.json`, see [`update_state_file`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Minimum time between two automatic checks (24 hours), in seconds.
pub const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// Whether Lector checks for new releases by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateCheck {
    /// Never, except when the user asks ("Check for updates now"). The default: Lector does nothing
    /// on a timer until the user opts in.
    #[default]
    Off,
    /// At most one request per 24 hours.
    On,
}

/// Which releases are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    /// Stable releases only.
    #[default]
    Stable,
    /// Stable releases and pre-releases.
    Preview,
}

/// `[updates]` (user settings only: a project's `.lector/settings.toml` cannot turn network
/// requests on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpdatesSettings {
    /// "off" (default) or "on".
    pub check: UpdateCheck,
    /// "stable" (default) or "preview".
    pub channel: UpdateChannel,
}

/// Whether an automatic check is due: `setting` is on and the last request was at least 24 hours
/// ago (or never, or its timestamp lies more than a day in the future, e.g. after a clock change).
/// Times are Unix seconds. A manual check ignores this.
pub fn should_check_now(last_checked: Option<i64>, now: i64, setting: UpdateCheck) -> bool {
    match (setting, last_checked) {
        (UpdateCheck::Off, _) => false,
        (UpdateCheck::On, None) => true,
        (UpdateCheck::On, Some(t)) => {
            now.saturating_sub(t) >= CHECK_INTERVAL_SECS
                || t.saturating_sub(now) > CHECK_INTERVAL_SECS
        }
    }
}

/// `<data dir>/update-state.json`: last check time, `ETag` and cached release list.
pub fn update_state_file() -> PathBuf {
    crate::paths::data_dir().join("update-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = CHECK_INTERVAL_SECS;

    #[test]
    fn schedule() {
        let now = 1_800_000_000;
        // Off never checks, whatever the history.
        assert!(!should_check_now(None, now, UpdateCheck::Off));
        assert!(!should_check_now(
            Some(now - 10 * DAY),
            now,
            UpdateCheck::Off
        ));
        // On: first run, then once a day.
        assert!(should_check_now(None, now, UpdateCheck::On));
        assert!(!should_check_now(Some(now), now, UpdateCheck::On));
        assert!(!should_check_now(Some(now - DAY + 1), now, UpdateCheck::On));
        assert!(should_check_now(Some(now - DAY), now, UpdateCheck::On));
        assert!(should_check_now(
            Some(now - 400 * DAY),
            now,
            UpdateCheck::On
        ));
        // Clock moved back a little: wait; a timestamp far in the future is ignored.
        assert!(!should_check_now(Some(now + 3600), now, UpdateCheck::On));
        assert!(should_check_now(Some(now + 2 * DAY), now, UpdateCheck::On));
        assert!(should_check_now(Some(i64::MIN), i64::MAX, UpdateCheck::On));
    }

    #[test]
    fn defaults_and_parsing() {
        let d = UpdatesSettings::default();
        assert_eq!(
            (d.check, d.channel),
            (UpdateCheck::Off, UpdateChannel::Stable)
        );
        let s: UpdatesSettings =
            toml::from_str("check = \"on\"\nchannel = \"preview\"\n").expect("parse");
        assert_eq!(
            (s.check, s.channel),
            (UpdateCheck::On, UpdateChannel::Preview)
        );
        let s: UpdatesSettings = toml::from_str("channel = \"preview\"").expect("parse");
        assert_eq!(s.check, UpdateCheck::Off);
        assert!(toml::from_str::<UpdatesSettings>("check = \"daily\"").is_err());
    }
}
