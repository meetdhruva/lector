//! Update check (v0.7): is a newer Lector release available, which file fits this machine, and
//! can its checksum be verified independently of the operating system's code signing?
//!
//! Notify only: nothing is downloaded or run here except two tiny text files (`SHA256SUMS.txt`
//! and its `.minisig`) when an update exists. The UI shows "Lector X is available" and opens the
//! release page or the asset URL in the browser. Lector never updates itself.
//!
//! * [`check_for_update`] / [`check_for_update_with`] ask the GitHub Releases API (unauthenticated,
//!   no user data, `User-Agent: lector/<version>`), compare versions with semver rules and return an
//!   [`UpdateStatus`]. Network failure and rate limiting are a quiet
//!   [`UpdateStatus::CouldNotCheck`], never an error to show prominently.
//! * [`UpdateState`] keeps the last check time, the `ETag` and the last parsed releases, so a daily
//!   check is normally an `If-None-Match` request answered with `304` (which GitHub does not count
//!   against the rate limit). The caller stores it in Lector's own data directory.
//! * [`InstallKind`] / [`detect_install_kind`] decide how this copy of Lector was installed; package
//!   manager installs get "update with brew / flatpak" instead of a download ([`UpdateAction`]).
//! * [`verify_checksums`] verifies `SHA256SUMS.txt` against the project's embedded minisign public key
//!   ([`UPDATE_PUBLIC_KEY`], file `packaging/update-key.pub`). While that file is the placeholder, nothing
//!   is ever reported as verified.
//!
//! # Trust model
//!
//! Three independent layers: HTTPS to GitHub (rustls with the platform verifier); the operating
//! systems' signatures (macOS Developer ID and notarization, Windows Authenticode, GPG `.asc`
//! files on Linux; optional, the release notes say what was signed, see `docs/RELEASING.md`);
//! and the project's own minisign signature of `SHA256SUMS.txt`, which is what
//! `UpdateStatus::Available::verified` means. `verified` is true only when a real key is
//! compiled in and [`verify_checksums`] accepts the file for this release's tag; legacy
//! non-prehashed signatures are refused, and there is no "unknown" middle state: an unverified
//! checksum may be shown only as an unsigned checksum. Key rotation means shipping a Lector with
//! the new public key first. Not covered: a stolen signing key, or a compromised build pipeline
//! that holds it.
//!
//! # Request and selection
//!
//! `GET {api}/repos/meetdhruva/lector/releases?per_page=30` without credentials or cookies,
//! 20 s timeout, at most 8 MiB. `403`/`429` with an exhausted rate limit or `Retry-After` is
//! [`CheckFailure::RateLimited`]; a connection failure leaves `last_checked` untouched while
//! rate limits and HTTP errors count as a check. Drafts, non-semver tags and build metadata are
//! ignored; [`Channel::Stable`] takes only non-prereleases; the candidate must be strictly
//! greater than the running version, so a downgrade is never offered. The checksum file
//! (at most 256 KiB) and its signature (at most 8 KiB) are fetched only when an update exists,
//! and only from `https://github.com` or the configured API origin.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{is_loopback, origin};
use crate::time::{now_secs, parse_rfc3339};

/// GitHub repository whose releases are checked.
pub const RELEASES_REPO: &str = "meetdhruva/lector";
/// Production API base URL.
pub const GITHUB_API: &str = "https://api.github.com";
/// The project's update-signing public key file (`packaging/update-key.pub`), embedded at build time.
/// While it holds only the placeholder comment, [`embedded_public_key`] is `None` and no download is
/// ever reported as verified.
pub const UPDATE_PUBLIC_KEY: &str = include_str!("../../../packaging/update-key.pub");
/// Name of the signed checksum file attached to every release.
pub const CHECKSUMS_NAME: &str = "SHA256SUMS.txt";
/// Name of its minisign signature.
pub const CHECKSUMS_SIGNATURE_NAME: &str = "SHA256SUMS.txt.minisig";
/// Application id (Flatpak id, bundle id).
pub const APP_ID: &str = "io.github.meetdhruva.Lector";

const MAX_RELEASES_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SUMS_BYTES: u64 = 256 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 8 * 1024;

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// Which releases are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Stable releases only (no pre-release versions).
    #[default]
    Stable,
    /// Stable releases and pre-releases (`0.8.0-beta.1`).
    Preview,
}

/// A file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    /// File name.
    pub name: String,
    /// Browser download URL.
    pub url: String,
    /// Size in bytes (0 if unknown).
    pub size: u64,
    /// SHA-256 (lowercase hex) listed in the release's `SHA256SUMS.txt`, when that file could be
    /// fetched. Only trustworthy when the surrounding [`UpdateStatus::Available::verified`] is true.
    pub sha256: Option<String>,
}

/// A published release (drafts are kept so the cache mirrors the API; selection ignores them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Git tag (`v0.7.0`).
    pub tag: String,
    /// Draft release (never offered).
    pub draft: bool,
    /// Pre-release flag from the host.
    pub prerelease: bool,
    /// Release page.
    pub html_url: String,
    /// Publication time, Unix seconds.
    pub published_at: Option<i64>,
    /// Attached files.
    pub assets: Vec<ReleaseAsset>,
}

/// Why a check produced no answer. Shown quietly (status line, log), never as a dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum CheckFailure {
    /// The API's rate limit is exhausted.
    #[error("rate limited by GitHub")]
    RateLimited {
        /// Unix seconds when requests work again, if the server said.
        retry_at: Option<i64>,
    },
    /// Connection, TLS or proxy failure.
    #[error("network: {0}")]
    Network(String),
    /// Any other HTTP status.
    #[error("GitHub answered HTTP {0}")]
    Http(u16),
    /// The response could not be understood.
    #[error("unexpected response: {0}")]
    BadResponse(String),
    /// The running version is not a semantic version.
    #[error("cannot compare version {0:?}")]
    InvalidVersion(String),
}

/// How this copy of Lector was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    /// `Lector.app` from the `.dmg` / `.zip` (or run from a mounted image).
    MacApp,
    /// Installed by Homebrew (the cask).
    Homebrew,
    /// Inno Setup installer (`unins000.exe` next to the executable).
    WindowsSetup,
    /// MSI (per-machine, under Program Files, no Inno uninstaller).
    WindowsMsi,
    /// Portable zip (`lector-data` next to the executable).
    WindowsPortable,
    /// `.deb` (executable in `/usr/bin` on a Debian-family system).
    Deb,
    /// `.rpm` (executable in `/usr/bin` on a Fedora / RHEL / SUSE-family system).
    Rpm,
    /// AppImage (`$APPIMAGE` is set).
    AppImage,
    /// Portable tarball with `install.sh` (`~/.local` or `/usr/local`).
    Tarball,
    /// Flatpak sandbox.
    Flatpak,
    /// Could not tell (cargo build, distribution package, unusual location).
    Unknown,
}

impl InstallKind {
    /// The command that updates a package-manager install, if this is one.
    pub fn update_command(self) -> Option<&'static str> {
        match self {
            InstallKind::Homebrew => Some("brew upgrade --cask lector"),
            InstallKind::Flatpak => Some("flatpak update io.github.meetdhruva.Lector"),
            _ => None,
        }
    }
}

/// Operating system and CPU architecture a release asset is chosen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Platform {
    /// Operating system.
    pub os: Os,
    /// CPU architecture.
    pub arch: Arch,
}

/// Operating systems with release assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    /// macOS.
    Macos,
    /// Windows.
    Windows,
    /// Linux.
    Linux,
    /// Anything else (no assets are offered).
    Other,
}

/// CPU architectures with release assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    /// x86-64 / AMD64.
    X86_64,
    /// ARM64 / AArch64.
    Aarch64,
    /// Anything else (no assets are offered).
    Other,
}

impl Platform {
    /// The platform this binary was compiled for.
    pub fn current() -> Platform {
        Platform {
            os: if cfg!(target_os = "macos") {
                Os::Macos
            } else if cfg!(target_os = "windows") {
                Os::Windows
            } else if cfg!(target_os = "linux") {
                Os::Linux
            } else {
                Os::Other
            },
            arch: if cfg!(target_arch = "x86_64") {
                Arch::X86_64
            } else if cfg!(target_arch = "aarch64") {
                Arch::Aarch64
            } else {
                Arch::Other
            },
        }
    }
}

/// What the UI should offer for an available update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateAction {
    /// Open this asset's URL in the browser (the user installs it themselves).
    Download {
        /// The file that matches this install.
        asset: ReleaseAsset,
    },
    /// Tell the user to update with their package manager.
    PackageManager {
        /// The command to run.
        command: String,
    },
    /// Open the release page (install kind unknown or no matching asset).
    ReleasePage,
}

/// Result of an update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// Nothing newer on this channel.
    UpToDate,
    /// A newer release exists.
    Available {
        /// Version without the leading `v` (`0.7.1`).
        version: String,
        /// Release notes page.
        notes_url: String,
        /// Publication time, Unix seconds.
        published_at: Option<i64>,
        /// Whether this is a pre-release.
        prerelease: bool,
        /// All files of the release; `sha256` is filled from `SHA256SUMS.txt`.
        assets: Vec<ReleaseAsset>,
        /// True only when `SHA256SUMS.txt` carries a valid minisign signature from the embedded
        /// project key for this release. False when unsigned, no key is configured, or anything failed.
        verified: bool,
        /// How this install was detected.
        install_kind: InstallKind,
        /// What to offer.
        action: UpdateAction,
        /// Extra advice, e.g. the `winget upgrade` command on Windows installers.
        hint: Option<String>,
    },
    /// The check did not produce an answer (offline, rate limited, ...). Keep the last known state.
    CouldNotCheck {
        /// Why.
        reason: CheckFailure,
    },
}

/// Settings of one check. [`UpdateOptions::default`] is the production configuration.
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// API base URL (tests point it at a local server).
    pub api_base: String,
    /// `owner/name`.
    pub repo: String,
    /// Minisign public key (file contents or the bare base64 line). `None`: cannot verify.
    pub public_key: Option<String>,
    /// Platform to pick assets for.
    pub platform: Platform,
    /// Install kind to pick assets for.
    pub install_kind: InstallKind,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        UpdateOptions {
            api_base: GITHUB_API.to_string(),
            repo: RELEASES_REPO.to_string(),
            public_key: embedded_public_key_text(),
            platform: Platform::current(),
            install_kind: detect_install_kind(&InstallProbe::from_environment()),
        }
    }
}

/// What the update check remembers between runs. A plain value: the caller persists it
/// (Lector keeps it in its state store, `lector_app::store`, concern `update_state`); this
/// crate opens no file for it. Unknown fields are ignored and missing ones default, so a
/// state written by another version still reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// Unix seconds of the last request that reached GitHub (answered or rate limited).
    pub last_checked: Option<i64>,
    /// `ETag` of the cached release list.
    pub etag: Option<String>,
    /// The release list that `etag` belongs to.
    pub releases: Vec<Release>,
}

// ---------------------------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------------------------

/// Parses `v0.7.0`, `0.7.0-beta.1` or `0.7.0+build` (build metadata is dropped: it never affects
/// precedence).
fn parse_version(text: &str) -> Option<semver::Version> {
    let t = text.trim();
    let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
    let mut v = semver::Version::parse(t).ok()?;
    v.build = semver::BuildMetadata::EMPTY;
    Some(v)
}

/// Semver precedence of two version strings (`v` prefix and build metadata ignored); `None` if
/// either is not a semantic version.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(parse_version(a)?.cmp(&parse_version(b)?))
}

/// The newest non-draft release on `channel` that is strictly newer than `current`.
/// Pre-releases only on [`Channel::Preview`]; never a downgrade; unparsable tags are skipped.
pub fn pick_release<'a>(
    releases: &'a [Release],
    current: &str,
    channel: Channel,
) -> Option<&'a Release> {
    let current = parse_version(current)?;
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| parse_version(&r.tag).map(|v| (r, v)))
        .filter(|(r, v)| match channel {
            Channel::Stable => !r.prerelease && v.pre.is_empty(),
            Channel::Preview => true,
        })
        .filter(|(_, v)| *v > current)
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(r, _)| r)
}

// ---------------------------------------------------------------------------------------------
// Install kind and asset selection
// ---------------------------------------------------------------------------------------------

/// Everything [`detect_install_kind`] looks at, gathered by [`InstallProbe::from_environment`] so
/// the decision itself is a pure, testable function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallProbe {
    /// Platform.
    pub platform: Platform,
    /// Path of the running executable (any separator).
    pub exe: String,
    /// `$APPIMAGE` is set (Linux).
    pub appimage: bool,
    /// Running inside a Flatpak sandbox (`$FLATPAK_ID` set or `/.flatpak-info` exists).
    pub flatpak: bool,
    /// `/etc/os-release` `ID` / `ID_LIKE` values, lowercase (Linux).
    pub distro_ids: Vec<String>,
    /// A Homebrew Caskroom entry for `lector` exists (macOS).
    pub homebrew_cask: bool,
    /// A `lector-data` directory sits next to the executable (portable mode).
    pub portable_dir: bool,
    /// `unins000.exe` sits next to the executable (Inno Setup install).
    pub inno_uninstaller: bool,
    /// The user's home directory (for `~/.local/bin`), if known.
    pub home: Option<String>,
}

impl InstallProbe {
    /// Reads the environment. Cheap: a few `stat` calls and one small file.
    pub fn from_environment() -> InstallProbe {
        let platform = Platform::current();
        let exe_path = std::env::current_exe()
            .ok()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
        let exe = exe_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = exe_path.as_deref().and_then(Path::parent);
        let flatpak = std::env::var_os("FLATPAK_ID").is_some_and(|v| !v.is_empty())
            || Path::new("/.flatpak-info").exists();
        let distro_ids = if platform.os == Os::Linux {
            std::fs::read_to_string("/etc/os-release")
                .map(|t| os_release_ids(&t))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        InstallProbe {
            platform,
            exe,
            appimage: std::env::var_os("APPIMAGE").is_some_and(|v| !v.is_empty()),
            flatpak,
            distro_ids,
            homebrew_cask: [
                "/opt/homebrew/Caskroom/lector",
                "/usr/local/Caskroom/lector",
            ]
            .iter()
            .any(|p| Path::new(p).is_dir()),
            portable_dir: dir.is_some_and(|d| d.join("lector-data").is_dir()),
            inno_uninstaller: dir.is_some_and(|d| d.join("unins000.exe").is_file()),
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| h.to_string_lossy().into_owned()),
        }
    }
}

/// `ID` and `ID_LIKE` values of an `os-release` file, lowercase.
pub fn os_release_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "ID" || key == "ID_LIKE" {
            out.extend(
                value
                    .trim()
                    .trim_matches(['"', '\''])
                    .split_whitespace()
                    .map(str::to_ascii_lowercase),
            );
        }
    }
    out
}

/// Decides how Lector was installed from what [`InstallProbe`] observed. When unsure the answer is
/// [`InstallKind::Unknown`], and the UI links to the release page.
pub fn detect_install_kind(p: &InstallProbe) -> InstallKind {
    let exe = p.exe.replace('\\', "/").to_ascii_lowercase();
    match p.platform.os {
        Os::Macos => {
            if p.homebrew_cask || exe.contains("/cellar/") || exe.contains("/caskroom/") {
                InstallKind::Homebrew
            } else if exe.contains(".app/contents/macos/") {
                InstallKind::MacApp
            } else {
                InstallKind::Unknown
            }
        }
        Os::Windows => {
            if p.portable_dir {
                InstallKind::WindowsPortable
            } else if p.inno_uninstaller {
                InstallKind::WindowsSetup
            } else if exe.contains("/program files") {
                InstallKind::WindowsMsi
            } else {
                InstallKind::Unknown
            }
        }
        Os::Linux => {
            let home = p
                .home
                .as_deref()
                .map(|h| h.trim_end_matches('/').to_ascii_lowercase());
            let has = |id: &str| p.distro_ids.iter().any(|d| d == id);
            if p.flatpak {
                InstallKind::Flatpak
            } else if p.appimage {
                InstallKind::AppImage
            } else if exe.starts_with("/usr/bin/") {
                if has("debian") || has("ubuntu") {
                    InstallKind::Deb
                } else if has("fedora") || has("rhel") || has("suse") || has("opensuse") {
                    InstallKind::Rpm
                } else {
                    InstallKind::Unknown
                }
            } else if exe.starts_with("/usr/local/bin/")
                || home
                    .as_deref()
                    .is_some_and(|h| exe == format!("{h}/.local/bin/lector"))
            {
                // `install.sh` from the portable tarball: `~/.local` or `/usr/local`.
                InstallKind::Tarball
            } else {
                InstallKind::Unknown
            }
        }
        Os::Other => InstallKind::Unknown,
    }
}

fn arch_matches(name: &str, arch: Arch) -> bool {
    let has = |needles: &[&str]| needles.iter().any(|n| name.contains(n));
    match arch {
        Arch::X86_64 => has(&["x86_64", "amd64", "x64"]),
        Arch::Aarch64 => has(&["aarch64", "arm64"]),
        Arch::Other => false,
    }
}

/// The release file that fits this platform and install kind, or `None` (package-manager installs,
/// unknown installs, missing file).
///
/// Names follow `scripts/package-*.sh|ps1`: `Lector-V-macos-universal.dmg`,
/// `Lector-V-windows-ARCH-setup.exe` / `.msi` / `-portable.zip`, `lector_V_ARCH.deb`,
/// `lector-V-1.ARCH.rpm`, `Lector-V-ARCH.AppImage`, `lector-V-linux-ARCH.tar.gz`.
pub fn select_asset(
    assets: &[ReleaseAsset],
    platform: Platform,
    kind: InstallKind,
) -> Option<&ReleaseAsset> {
    let find = |os_token: &str, suffix: &str, per_arch: bool| {
        assets.iter().find(|a| {
            let n = a.name.to_ascii_lowercase();
            n.ends_with(suffix)
                && (os_token.is_empty() || n.contains(os_token))
                && (!per_arch || arch_matches(&n, platform.arch))
        })
    };
    match (platform.os, kind) {
        (Os::Macos, InstallKind::MacApp) => find("macos", ".dmg", false),
        (Os::Windows, InstallKind::WindowsSetup) => find("windows", "-setup.exe", true),
        (Os::Windows, InstallKind::WindowsMsi) => find("windows", ".msi", true),
        (Os::Windows, InstallKind::WindowsPortable) => find("windows", "-portable.zip", true),
        (Os::Linux, InstallKind::Deb) => find("", ".deb", true),
        (Os::Linux, InstallKind::Rpm) => find("", ".rpm", true),
        (Os::Linux, InstallKind::AppImage) => find("", ".appimage", true),
        (Os::Linux, InstallKind::Tarball) => find("linux", ".tar.gz", true),
        _ => None,
    }
}

/// What to offer: package managers get their command, other known installs the matching file,
/// everything else the release page.
pub fn plan_action(assets: &[ReleaseAsset], platform: Platform, kind: InstallKind) -> UpdateAction {
    if let Some(command) = kind.update_command() {
        return UpdateAction::PackageManager {
            command: command.to_string(),
        };
    }
    match select_asset(assets, platform, kind) {
        Some(asset) => UpdateAction::Download {
            asset: asset.clone(),
        },
        None => UpdateAction::ReleasePage,
    }
}

/// Windows installers cannot be told apart from winget installs; mention the command.
fn hint_for(kind: InstallKind) -> Option<String> {
    match kind {
        InstallKind::WindowsSetup | InstallKind::WindowsMsi => Some(
            "Installed with winget? Run `winget upgrade meetdhruva.Lector` instead.".to_string(),
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Checksums and signatures
// ---------------------------------------------------------------------------------------------

/// The embedded public key text, or `None` while `packaging/update-key.pub` is the placeholder.
pub fn embedded_public_key_text() -> Option<String> {
    public_key_line(UPDATE_PUBLIC_KEY).map(str::to_string)
}

/// Whether a real update-signing key is compiled in.
pub fn embedded_public_key() -> bool {
    embedded_public_key_text().is_some()
}

/// The base64 line of a minisign public key file: the first line that is not blank, a `#` comment
/// or an `untrusted comment:` line.
fn public_key_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| {
        !l.is_empty()
            && !l.starts_with('#')
            && !l.to_ascii_lowercase().starts_with("untrusted comment:")
    })
}

/// Parses `sha256sum` output (`<hex>  <name>` or `<hex> *<name>`) into name → lowercase hex.
pub fn parse_checksums(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hash, rest)) = line.split_once(' ') else {
            continue;
        };
        let name = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*'));
        let (Some(name), true) = (
            name,
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        ) else {
            continue;
        };
        out.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    out
}

/// Verifies `signature` (a `.minisig` file) over `sums` with the minisign public key `public_key`.
///
/// True only when a key is configured and parses, the signature is a valid prehashed (`ED`)
/// minisign signature by that key over exactly these bytes, and its trusted comment is
/// `lector <tag>`, which binds the signature to this release (an old, validly signed file cannot be
/// replayed under a new tag).
pub fn verify_checksums(
    public_key: Option<&str>,
    sums: &[u8],
    signature: Option<&str>,
    tag: &str,
) -> bool {
    let (Some(key), Some(sig)) = (public_key, signature) else {
        return false;
    };
    let Some(line) = public_key_line(key) else {
        return false;
    };
    let Ok(key) = minisign_verify::PublicKey::from_base64(line) else {
        return false;
    };
    let Ok(sig) = minisign_verify::Signature::decode(sig) else {
        return false;
    };
    key.verify(sums, &sig, false).is_ok() && sig.trusted_comment() == format!("lector {tag}")
}

// ---------------------------------------------------------------------------------------------
// Releases API
// ---------------------------------------------------------------------------------------------

/// Parses the body of `GET /repos/{owner}/{repo}/releases`. Entries without a tag are skipped.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, CheckFailure> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| CheckFailure::BadResponse(e.to_string()))?;
    let Value::Array(items) = value else {
        return Err(CheckFailure::BadResponse("expected a JSON array".into()));
    };
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(items
        .iter()
        .filter_map(|r| {
            let tag = text(r, "tag_name")?;
            let assets = r
                .get("assets")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|a| {
                            Some(ReleaseAsset {
                                name: text(a, "name")?,
                                url: text(a, "browser_download_url")?,
                                size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
                                sha256: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Release {
                html_url: text(r, "html_url").unwrap_or_else(|| {
                    format!("https://github.com/{RELEASES_REPO}/releases/tag/{tag}")
                }),
                draft: r.get("draft").and_then(Value::as_bool).unwrap_or(false),
                prerelease: r
                    .get("prerelease")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                published_at: text(r, "published_at").and_then(|t| parse_rfc3339(&t)),
                assets,
                tag,
            })
        })
        .collect())
}

fn agent(api_base: &str) -> ureq::Agent {
    let mut cfg = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .user_agent(concat!("lector/", env!("CARGO_PKG_VERSION")))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        );
    if is_loopback(api_base) {
        cfg = cfg.proxy(None);
    }
    cfg.build().into()
}

enum Fetched {
    Fresh { body: String, etag: Option<String> },
    NotModified,
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn fetch_releases(
    agent: &ureq::Agent,
    url: &str,
    etag: Option<&str>,
) -> Result<Fetched, CheckFailure> {
    let mut req = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(etag) = etag {
        req = req.header("If-None-Match", etag);
    }
    let mut resp = req
        .call()
        .map_err(|e| CheckFailure::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    tracing::debug!(target: "lector_forge", status, "update check");
    match status {
        200 => {
            let etag = header(&resp, "etag");
            let body = resp
                .body_mut()
                .with_config()
                .limit(MAX_RELEASES_BYTES)
                .read_to_string()
                .map_err(|e| CheckFailure::Network(e.to_string()))?;
            Ok(Fetched::Fresh { body, etag })
        }
        304 => Ok(Fetched::NotModified),
        403 | 429 => {
            let remaining = header(&resp, "x-ratelimit-remaining");
            let retry_after = header(&resp, "retry-after").and_then(|v| v.parse::<i64>().ok());
            let reset = header(&resp, "x-ratelimit-reset").and_then(|v| v.parse::<i64>().ok());
            let limited =
                status == 429 || remaining.as_deref() == Some("0") || retry_after.is_some();
            if limited {
                Err(CheckFailure::RateLimited {
                    retry_at: retry_after.map(|s| now_secs() + s).or(reset),
                })
            } else {
                Err(CheckFailure::Http(status))
            }
        }
        other => Err(CheckFailure::Http(other)),
    }
}

/// May `url` (from the API response) be fetched? HTTPS on github.com, or the configured API origin
/// (local test servers).
fn download_allowed(url: &str, api_base: &str) -> bool {
    let o = origin(url);
    o == "https://github.com" || o == origin(api_base)
}

fn download(agent: &ureq::Agent, url: &str, limit: u64) -> Option<Vec<u8>> {
    let mut resp = agent.get(url).call().ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    resp.body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .ok()
}

/// Fetches and checks `SHA256SUMS.txt` of `release`; fills `assets[].sha256`; returns `verified`.
fn verify_release(
    agent: &ureq::Agent,
    release: &Release,
    assets: &mut [ReleaseAsset],
    opts: &UpdateOptions,
) -> bool {
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name && download_allowed(&a.url, &opts.api_base))
    };
    let Some(sums_asset) = find(CHECKSUMS_NAME) else {
        return false;
    };
    let Some(sums) = download(agent, &sums_asset.url, MAX_SUMS_BYTES) else {
        return false;
    };
    let table = parse_checksums(&String::from_utf8_lossy(&sums));
    for a in assets.iter_mut() {
        a.sha256 = table.get(&a.name).cloned();
    }
    let signature = find(CHECKSUMS_SIGNATURE_NAME)
        .and_then(|a| download(agent, &a.url, MAX_SIGNATURE_BYTES))
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    verify_checksums(
        opts.public_key.as_deref(),
        &sums,
        signature.as_deref(),
        &release.tag,
    )
}

/// Checks for an update with production settings and no persistent state (always a full request).
/// Blocking: call from a background thread.
pub fn check_for_update(current_version: &str, channel: Channel) -> UpdateStatus {
    check_for_update_with(
        current_version,
        channel,
        &UpdateOptions::default(),
        &mut UpdateState::default(),
    )
}

/// Checks for an update. `state` supplies the `ETag` and cached releases and receives the new ones
/// plus `last_checked`; the caller saves it ([`UpdateState::save`]). Blocking.
///
/// A `304` is answered from `state.releases`. A request that never reached GitHub (connection
/// failure) leaves `last_checked` untouched; rate limiting and HTTP errors count as a check.
pub fn check_for_update_with(
    current_version: &str,
    channel: Channel,
    opts: &UpdateOptions,
    state: &mut UpdateState,
) -> UpdateStatus {
    if parse_version(current_version).is_none() {
        return UpdateStatus::CouldNotCheck {
            reason: CheckFailure::InvalidVersion(current_version.to_string()),
        };
    }
    let agent = agent(&opts.api_base);
    let url = format!(
        "{}/repos/{}/releases?per_page=30",
        opts.api_base.trim_end_matches('/'),
        opts.repo
    );
    let etag = state.etag.as_deref().filter(|_| !state.releases.is_empty());
    let fetched = match fetch_releases(&agent, &url, etag) {
        Ok(Fetched::NotModified) if etag.is_none() => {
            return UpdateStatus::CouldNotCheck {
                reason: CheckFailure::BadResponse("304 to an unconditional request".into()),
            };
        }
        Ok(f) => f,
        Err(reason) => {
            if matches!(
                reason,
                CheckFailure::RateLimited { .. } | CheckFailure::Http(_)
            ) {
                state.last_checked = Some(now_secs());
            }
            return UpdateStatus::CouldNotCheck { reason };
        }
    };
    state.last_checked = Some(now_secs());
    if let Fetched::Fresh { body, etag } = fetched {
        match parse_releases(&body) {
            Ok(releases) => {
                state.releases = releases;
                state.etag = etag;
            }
            Err(reason) => return UpdateStatus::CouldNotCheck { reason },
        }
    }
    status_from(&agent, &state.releases, current_version, channel, opts)
}

/// Selection and verification on an already known release list.
fn status_from(
    agent: &ureq::Agent,
    releases: &[Release],
    current_version: &str,
    channel: Channel,
    opts: &UpdateOptions,
) -> UpdateStatus {
    let Some(release) = pick_release(releases, current_version, channel) else {
        return UpdateStatus::UpToDate;
    };
    let mut assets = release.assets.clone();
    let verified = verify_release(agent, release, &mut assets, opts);
    let action = plan_action(&assets, opts.platform, opts.install_kind);
    UpdateStatus::Available {
        version: release.tag.trim_start_matches(['v', 'V']).to_string(),
        notes_url: release.html_url.clone(),
        published_at: release.published_at,
        prerelease: release.prerelease,
        assets,
        verified,
        install_kind: opts.install_kind,
        action,
        hint: hint_for(opts.install_kind),
    }
}

#[cfg(test)]
mod tests;
