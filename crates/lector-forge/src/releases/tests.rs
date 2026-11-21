//! Unit tests of the update check: versions, install kinds, asset choice, checksums and release parsing.

use super::*;
use crate::http::base64;
use crate::mock::{MockServer, Route};
use blake2::{Blake2b512, Digest};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

const FIXTURE: &str = include_str!("../../tests/fixtures/releases/releases.json");
const RELEASES_PATH: &str = "/repos/meetdhruva/lector/releases";

/// A throw-away minisign-compatible key pair generated for the test.
struct TestKey {
    pair: Ed25519KeyPair,
    key_id: [u8; 8],
}

impl TestKey {
    fn generate() -> TestKey {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("generate");
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("pkcs8");
        let mut key_id = [0u8; 8];
        ring::rand::SecureRandom::fill(&rng, &mut key_id).expect("random");
        TestKey { pair, key_id }
    }

    /// The base64 line of the `.pub` file.
    fn public(&self) -> String {
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.key_id);
        raw.extend_from_slice(self.pair.public_key().as_ref());
        base64(&raw)
    }

    /// A `.minisig` file for `message`; `prehashed` = what minisign 0.10+ writes by default.
    fn sign(&self, message: &[u8], trusted_comment: &str, prehashed: bool) -> String {
        let signed: Vec<u8> = if prehashed {
            Blake2b512::digest(message).to_vec()
        } else {
            message.to_vec()
        };
        let sig = self.pair.sign(&signed);
        let mut block = if prehashed {
            b"ED".to_vec()
        } else {
            b"Ed".to_vec()
        };
        block.extend_from_slice(&self.key_id);
        block.extend_from_slice(sig.as_ref());
        let mut global = sig.as_ref().to_vec();
        global.extend_from_slice(trusted_comment.as_bytes());
        let global_sig = self.pair.sign(&global);
        format!(
            "untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {}\n{}\n",
            base64(&block),
            trusted_comment,
            base64(global_sig.as_ref())
        )
    }
}

fn releases() -> Vec<Release> {
    parse_releases(FIXTURE).expect("fixture parses")
}

fn deb_x64() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

// --- versions ---------------------------------------------------------------------------

#[test]
fn version_comparison_table() {
    use Ordering::*;
    let table = [
        ("0.7.0", "0.6.0", Greater),
        ("v0.7.0", "0.7.0", Equal),
        ("0.7.0", "0.7.0-beta.1", Greater),
        ("0.7.0-beta.2", "0.7.0-beta.10", Less),
        ("0.7.0-rc.1", "0.7.0-beta.9", Greater),
        ("0.10.0", "0.9.9", Greater),
        ("1.0.0", "0.99.99", Greater),
        ("0.7.0+abc", "0.7.0+def", Equal),
        ("0.7.1", "0.7.0", Greater),
    ];
    for (a, b, want) in table {
        assert_eq!(compare_versions(a, b), Some(want), "{a} vs {b}");
        assert_eq!(compare_versions(b, a), Some(want.reverse()), "{b} vs {a}");
    }
    assert_eq!(compare_versions("0.7", "0.7.0"), None);
    assert_eq!(compare_versions("nightly", "0.7.0"), None);
    assert_eq!(compare_versions("", "0.7.0"), None);
}

#[test]
fn release_selection_follows_channel_and_never_downgrades() {
    let r = releases();
    let tag = |current: &str, channel| pick_release(&r, current, channel).map(|r| r.tag.as_str());
    // Stable: newest stable, skipping the draft v0.7.2 and the pre-release.
    assert_eq!(tag("0.6.0", Channel::Stable), Some("v0.7.1"));
    assert_eq!(tag("0.7.0", Channel::Stable), Some("v0.7.1"));
    assert_eq!(tag("0.7.1", Channel::Stable), None);
    // Preview: pre-releases count, drafts still do not.
    assert_eq!(tag("0.7.1", Channel::Preview), Some("v0.8.0-beta.1"));
    assert_eq!(tag("0.8.0-beta.1", Channel::Preview), None);
    assert_eq!(tag("0.6.0", Channel::Preview), Some("v0.8.0-beta.1"));
    // A pre-release user on the stable channel moves to the final release of that version, but a
    // newer pre-release build is never "downgraded" to an older stable.
    assert_eq!(tag("0.7.1-rc.1", Channel::Stable), Some("v0.7.1"));
    assert_eq!(tag("0.9.0-dev.1", Channel::Stable), None);
    assert_eq!(tag("0.9.0-dev.1", Channel::Preview), None);
    // Unparsable current version: no answer.
    assert_eq!(tag("dev", Channel::Stable), None);
}

// --- install kind ---------------------------------------------------------------------

fn probe(os: Os, exe: &str) -> InstallProbe {
    InstallProbe {
        platform: Platform {
            os,
            arch: Arch::X86_64,
        },
        exe: exe.to_string(),
        appimage: false,
        flatpak: false,
        distro_ids: Vec::new(),
        homebrew_cask: false,
        portable_dir: false,
        inno_uninstaller: false,
        home: Some("/home/u".to_string()),
    }
}

#[test]
fn install_kind_detection() {
    use InstallKind::*;
    let mac = "/Applications/Lector.app/Contents/MacOS/lector";
    assert_eq!(detect_install_kind(&probe(Os::Macos, mac)), MacApp);
    let mut cask = probe(Os::Macos, mac);
    cask.homebrew_cask = true;
    assert_eq!(detect_install_kind(&cask), Homebrew);
    assert_eq!(
        detect_install_kind(&probe(
            Os::Macos,
            "/Users/u/src/lector/target/release/lector"
        )),
        Unknown
    );

    let win = r"C:\Program Files\Lector\lector.exe";
    assert_eq!(detect_install_kind(&probe(Os::Windows, win)), WindowsMsi);
    let mut inno = probe(
        Os::Windows,
        r"C:\Users\u\AppData\Local\Programs\Lector\lector.exe",
    );
    inno.inno_uninstaller = true;
    assert_eq!(detect_install_kind(&inno), WindowsSetup);
    let mut portable = probe(Os::Windows, r"D:\tools\Lector\lector.exe");
    portable.portable_dir = true;
    assert_eq!(detect_install_kind(&portable), WindowsPortable);
    assert_eq!(
        detect_install_kind(&probe(
            Os::Windows,
            r"D:\src\lector\target\release\lector.exe"
        )),
        Unknown
    );

    let linux = |exe: &str, ids: &[&str]| {
        let mut p = probe(Os::Linux, exe);
        p.distro_ids = ids.iter().map(|s| s.to_string()).collect();
        p
    };
    assert_eq!(
        detect_install_kind(&linux("/usr/bin/lector", &["ubuntu", "debian"])),
        Deb
    );
    assert_eq!(
        detect_install_kind(&linux("/usr/bin/lector", &["fedora"])),
        Rpm
    );
    assert_eq!(
        detect_install_kind(&linux(
            "/usr/bin/lector",
            &["opensuse-leap", "suse", "opensuse"]
        )),
        Rpm
    );
    assert_eq!(
        detect_install_kind(&linux("/usr/bin/lector", &["arch"])),
        Unknown
    );
    assert_eq!(
        detect_install_kind(&linux("/usr/local/bin/lector", &["arch"])),
        Tarball
    );
    assert_eq!(
        detect_install_kind(&linux("/home/u/.local/bin/lector", &[])),
        Tarball
    );
    assert_eq!(
        detect_install_kind(&linux("/home/u/.cargo/bin/lector", &[])),
        Unknown
    );
    let mut appimage = linux("/tmp/.mount_Lectorab12/usr/bin/lector", &["ubuntu"]);
    appimage.appimage = true;
    assert_eq!(detect_install_kind(&appimage), AppImage);
    let mut flatpak = linux("/app/bin/lector", &["org.freedesktop.platform"]);
    flatpak.flatpak = true;
    flatpak.appimage = true; // the sandbox wins
    assert_eq!(detect_install_kind(&flatpak), Flatpak);
}

#[test]
fn os_release_parsing() {
    let text = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n";
    assert_eq!(os_release_ids(text), ["ubuntu", "debian"]);
    let text = "ID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"\n";
    assert_eq!(os_release_ids(text), ["opensuse-leap", "suse", "opensuse"]);
    assert!(os_release_ids("").is_empty());
}

// --- asset selection -------------------------------------------------------------------

#[test]
fn asset_selection_per_platform_and_kind() {
    let r = releases();
    let v071 = r.iter().find(|r| r.tag == "v0.7.1").expect("v0.7.1");
    let pick = |os, arch, kind| {
        select_asset(&v071.assets, Platform { os, arch }, kind).map(|a| a.name.as_str())
    };
    use Arch::{Aarch64, X86_64};
    use InstallKind::*;
    use Os::{Linux, Macos, Windows};
    let table = [
        (
            Macos,
            Aarch64,
            MacApp,
            Some("Lector-0.7.1-macos-universal.dmg"),
        ),
        (
            Macos,
            X86_64,
            MacApp,
            Some("Lector-0.7.1-macos-universal.dmg"),
        ),
        (
            Windows,
            X86_64,
            WindowsSetup,
            Some("Lector-0.7.1-windows-x64-setup.exe"),
        ),
        (
            Windows,
            Aarch64,
            WindowsSetup,
            Some("Lector-0.7.1-windows-arm64-setup.exe"),
        ),
        (
            Windows,
            X86_64,
            WindowsMsi,
            Some("Lector-0.7.1-windows-x64.msi"),
        ),
        (
            Windows,
            Aarch64,
            WindowsMsi,
            Some("Lector-0.7.1-windows-arm64.msi"),
        ),
        (
            Windows,
            X86_64,
            WindowsPortable,
            Some("Lector-0.7.1-windows-x64-portable.zip"),
        ),
        (Linux, X86_64, Deb, Some("lector_0.7.1_amd64.deb")),
        (Linux, Aarch64, Deb, Some("lector_0.7.1_arm64.deb")),
        (Linux, X86_64, Rpm, Some("lector-0.7.1-1.x86_64.rpm")),
        (Linux, Aarch64, Rpm, Some("lector-0.7.1-1.aarch64.rpm")),
        (
            Linux,
            X86_64,
            AppImage,
            Some("Lector-0.7.1-x86_64.AppImage"),
        ),
        (
            Linux,
            Aarch64,
            AppImage,
            Some("Lector-0.7.1-aarch64.AppImage"),
        ),
        (
            Linux,
            X86_64,
            Tarball,
            Some("lector-0.7.1-linux-x86_64.tar.gz"),
        ),
        (
            Linux,
            Aarch64,
            Tarball,
            Some("lector-0.7.1-linux-aarch64.tar.gz"),
        ),
        // Mismatches and unknowns never guess.
        (Linux, X86_64, Unknown, None),
        (Macos, Aarch64, Deb, None),
        (Windows, Arch::Other, WindowsSetup, None),
        (Os::Other, X86_64, Unknown, None),
        (Linux, X86_64, Flatpak, None),
        (Macos, Aarch64, Homebrew, None),
    ];
    for (os, arch, kind, want) in table {
        assert_eq!(pick(os, arch, kind), want, "{os:?} {arch:?} {kind:?}");
    }
    // Signature files are never mistaken for the package.
    assert!(
        select_asset(&v071.assets, deb_x64(), AppImage).is_some_and(|a| !a.name.ends_with(".asc"))
    );
}

#[test]
fn actions() {
    let r = releases();
    let v071 = r.iter().find(|r| r.tag == "v0.7.1").expect("v0.7.1");
    let p = |os, arch| Platform { os, arch };
    assert_eq!(
        plan_action(
            &v071.assets,
            p(Os::Macos, Arch::Aarch64),
            InstallKind::Homebrew
        ),
        UpdateAction::PackageManager {
            command: "brew upgrade --cask lector".into()
        }
    );
    assert_eq!(
        plan_action(&v071.assets, deb_x64(), InstallKind::Flatpak),
        UpdateAction::PackageManager {
            command: "flatpak update io.github.meetdhruva.Lector".into()
        }
    );
    assert_eq!(
        plan_action(&v071.assets, deb_x64(), InstallKind::Unknown),
        UpdateAction::ReleasePage
    );
    assert!(matches!(
        plan_action(&v071.assets, deb_x64(), InstallKind::Deb),
        UpdateAction::Download { asset } if asset.name == "lector_0.7.1_amd64.deb"
    ));
    assert!(hint_for(InstallKind::WindowsSetup).is_some_and(|h| h.contains("winget upgrade")));
    assert!(hint_for(InstallKind::Deb).is_none());
}

// --- parsing ----------------------------------------------------------------------------

#[test]
fn parses_a_recorded_releases_response() {
    let r = releases();
    assert_eq!(r.len(), 7);
    let v071 = r.iter().find(|r| r.tag == "v0.7.1").expect("v0.7.1");
    assert!(!v071.draft && !v071.prerelease);
    assert_eq!(
        v071.html_url,
        "https://github.com/meetdhruva/lector/releases/tag/v0.7.1"
    );
    assert_eq!(v071.published_at, parse_rfc3339("2026-12-15T16:30:12Z"));
    assert_eq!(v071.assets.len(), 22);
    let sums = v071
        .assets
        .iter()
        .find(|a| a.name == CHECKSUMS_NAME)
        .expect("sums");
    assert_eq!(
        sums.url,
        "https://github.com/meetdhruva/lector/releases/download/v0.7.1/SHA256SUMS.txt"
    );
    assert!(r.iter().any(|r| r.tag == "v0.7.2" && r.draft));
    assert!(r.iter().any(|r| r.tag == "v0.8.0-beta.1" && r.prerelease));
    // A draft has no publication date.
    assert_eq!(
        r.iter()
            .find(|r| r.tag == "v0.7.2")
            .and_then(|r| r.published_at),
        None
    );

    assert!(matches!(
        parse_releases("{}"),
        Err(CheckFailure::BadResponse(_))
    ));
    assert!(matches!(
        parse_releases("<html>"),
        Err(CheckFailure::BadResponse(_))
    ));
    assert_eq!(parse_releases("[]").expect("empty"), Vec::new());
    // Entries without a tag are dropped, odd shapes tolerated.
    let odd = parse_releases(r#"[{"name":"x"},{"tag_name":"v1.0.0","assets":[{"name":"a"}]}]"#)
        .expect("odd");
    assert_eq!(odd.len(), 1);
    assert!(odd[0].assets.is_empty() && odd[0].published_at.is_none());
}

#[test]
fn parses_checksums() {
    let h1 = "a".repeat(64);
    let h2 = "B".repeat(64);
    let text = format!(
        "{h1}  Lector-0.7.1-macos-universal.dmg\r\n{h2} *lector_0.7.1_amd64.deb\nnot a hash  x\n{h1}\n\n{h1}  with space.zip\n"
    );
    let t = parse_checksums(&text);
    assert_eq!(t.len(), 3);
    assert_eq!(t["Lector-0.7.1-macos-universal.dmg"], h1);
    assert_eq!(
        t["lector_0.7.1_amd64.deb"],
        "b".repeat(64),
        "lowercased, binary marker stripped"
    );
    assert_eq!(t["with space.zip"], h1);
}

// --- signatures -------------------------------------------------------------------------

#[test]
fn signature_verification() {
    let key = TestKey::generate();
    let other = TestKey::generate();
    let sums = b"0000  file\n";
    let tag = "v0.7.1";
    let comment = "lector v0.7.1";
    let sig = key.sign(sums, comment, true);
    let pk = key.public();
    let pub_file = format!("untrusted comment: minisign public key\n{pk}\n");

    assert!(verify_checksums(Some(&pk), sums, Some(&sig), tag));
    assert!(
        verify_checksums(Some(&pub_file), sums, Some(&sig), tag),
        "full .pub file"
    );
    // Tampered content, other key, other tag, missing pieces, garbage.
    assert!(!verify_checksums(
        Some(&pk),
        b"0000  file2\n",
        Some(&sig),
        tag
    ));
    assert!(!verify_checksums(
        Some(&other.public()),
        sums,
        Some(&sig),
        tag
    ));
    assert!(
        !verify_checksums(Some(&pk), sums, Some(&sig), "v0.7.2"),
        "bound to the tag"
    );
    assert!(!verify_checksums(
        Some(&pk),
        sums,
        Some(&key.sign(sums, "something else", true)),
        tag
    ));
    assert!(!verify_checksums(Some(&pk), sums, None, tag));
    assert!(
        !verify_checksums(None, sums, Some(&sig), tag),
        "no key: cannot verify"
    );
    assert!(!verify_checksums(
        Some("# placeholder only\n"),
        sums,
        Some(&sig),
        tag
    ));
    assert!(!verify_checksums(
        Some("not base64!"),
        sums,
        Some(&sig),
        tag
    ));
    assert!(!verify_checksums(Some(&pk), sums, Some("garbage"), tag));
    // Legacy (non-prehashed) signatures are refused; so is a tampered global signature.
    assert!(!verify_checksums(
        Some(&pk),
        sums,
        Some(&key.sign(sums, comment, false)),
        tag
    ));
    let tampered = sig.replace(
        "trusted comment: lector v0.7.1",
        "trusted comment: lector v0.7.9",
    );
    assert!(!verify_checksums(
        Some(&pk),
        sums,
        Some(&tampered),
        "v0.7.9"
    ));
}

/// A signature made by the real `minisign` 0.11 tool with a throw-away key (the secret key was
/// discarded): the format we verify is the one the release workflow produces.
#[test]
fn verifies_a_signature_made_by_the_minisign_tool() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/releases/minisign"
    );
    let read = |name: &str| std::fs::read(format!("{dir}/{name}")).expect(name);
    let text = |name: &str| String::from_utf8(read(name)).expect(name);
    let (key, sums, sig) = (
        text("update-key.pub"),
        read("SHA256SUMS.txt"),
        text("SHA256SUMS.txt.minisig"),
    );
    assert!(verify_checksums(Some(&key), &sums, Some(&sig), "v0.7.0"));
    assert!(!verify_checksums(Some(&key), &sums, Some(&sig), "v0.7.1"));
    let mut tampered = sums.clone();
    tampered.push(b'\n');
    assert!(!verify_checksums(
        Some(&key),
        &tampered,
        Some(&sig),
        "v0.7.0"
    ));
    assert_eq!(parse_checksums(&String::from_utf8_lossy(&sums)).len(), 2);
}

#[test]
fn placeholder_key_means_cannot_verify() {
    assert_eq!(public_key_line("# a\n\n# b\n"), None);
    assert_eq!(
        public_key_line("untrusted comment: x\nRWQabc\n"),
        Some("RWQabc")
    );
    // Whatever is committed in packaging/update-key.pub must be a usable key or the placeholder.
    match embedded_public_key_text() {
        None => assert!(!embedded_public_key()),
        Some(line) => assert!(
            minisign_verify::PublicKey::from_base64(&line).is_ok(),
            "packaging/update-key.pub holds a malformed key"
        ),
    }
}

// --- network (local server) --------------------------------------------------------------

struct Fixture {
    server: MockServer,
    key: TestKey,
    opts: UpdateOptions,
}

/// Serves the recorded release list (download URLs rewritten to the local server) and a signed
/// `SHA256SUMS.txt` for every release.
fn fixture(sign_tag: &str) -> Fixture {
    let server = MockServer::start(0, Vec::new()).expect("server");
    let body = FIXTURE.replace(
        "https://github.com/meetdhruva/lector/releases/download",
        &format!("{}/dl", server.url()),
    );
    server.add(Route::json("GET", RELEASES_PATH, body).header("etag", "\"v1\""));
    let key = TestKey::generate();
    for r in releases() {
        let sums: String = r
            .assets
            .iter()
            .enumerate()
            .map(|(i, a)| format!("{:064x}  {}\n", i + 1, a.name))
            .collect();
        let tag = &r.tag;
        let comment = if tag == "v0.7.1" {
            format!("lector {sign_tag}")
        } else {
            format!("lector {tag}")
        };
        server.add(Route::json(
            "GET",
            &format!("/dl/{tag}/{CHECKSUMS_NAME}"),
            sums.clone(),
        ));
        server.add(Route::json(
            "GET",
            &format!("/dl/{tag}/{CHECKSUMS_SIGNATURE_NAME}"),
            key.sign(sums.as_bytes(), &comment, true),
        ));
    }
    let opts = UpdateOptions {
        api_base: server.url(),
        repo: RELEASES_REPO.to_string(),
        public_key: Some(key.public()),
        platform: deb_x64(),
        install_kind: InstallKind::Deb,
    };
    Fixture { server, key, opts }
}

#[test]
fn update_available_and_verified() {
    let f = fixture("v0.7.1");
    let mut state = UpdateState::default();
    let status = check_for_update_with("0.7.0", Channel::Stable, &f.opts, &mut state);
    let UpdateStatus::Available {
        version,
        notes_url,
        verified,
        assets,
        action,
        install_kind,
        prerelease,
        published_at,
        hint,
    } = status
    else {
        panic!("expected an update, got {status:?}");
    };
    assert_eq!(version, "0.7.1");
    assert!(notes_url.ends_with("/releases/tag/v0.7.1"));
    assert!(verified && !prerelease && hint.is_none());
    assert_eq!(install_kind, InstallKind::Deb);
    assert_eq!(published_at, parse_rfc3339("2026-12-15T16:30:12Z"));
    let deb = assets
        .iter()
        .find(|a| a.name == "lector_0.7.1_amd64.deb")
        .expect("deb");
    assert_eq!(deb.sha256.as_ref().map(String::len), Some(64));
    assert_eq!(action, UpdateAction::Download { asset: deb.clone() });
    assert_eq!(state.etag.as_deref(), Some("\"v1\""));
    assert!(state.last_checked.is_some() && !state.releases.is_empty());

    // No user data leaves the machine: a plain user agent, no credentials, no cookies.
    let reqs = f.server.requests();
    let first = &reqs[0];
    assert_eq!(
        first.header("user-agent"),
        Some(concat!("lector/", env!("CARGO_PKG_VERSION")))
    );
    assert!(first.header("authorization").is_none() && first.header("cookie").is_none());
    assert!(first.header("if-none-match").is_none());
    let _ = &f.key;
}

#[test]
fn unsigned_or_unconfigured_is_not_verified() {
    // No public key configured: hashes are shown, `verified` stays false.
    let mut f = fixture("v0.7.1");
    f.opts.public_key = None;
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    let UpdateStatus::Available {
        verified, assets, ..
    } = status
    else {
        panic!("expected an update");
    };
    assert!(!verified);
    assert!(assets.iter().any(|a| a.sha256.is_some()));

    // Signature bound to another tag (replay of an old signed file).
    let f = fixture("v0.7.0");
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    assert!(matches!(
        status,
        UpdateStatus::Available {
            verified: false,
            ..
        }
    ));

    // Wrong key.
    let mut f = fixture("v0.7.1");
    f.opts.public_key = Some(TestKey::generate().public());
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    assert!(matches!(
        status,
        UpdateStatus::Available {
            verified: false,
            ..
        }
    ));

    // Release without a signature file.
    let f = fixture("v0.7.1");
    f.server.add(
        Route::json(
            "GET",
            &format!("/dl/v0.7.1/{CHECKSUMS_SIGNATURE_NAME}"),
            "nope",
        )
        .status(404),
    );
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    assert!(matches!(
        status,
        UpdateStatus::Available {
            verified: false,
            ..
        }
    ));

    // Sums file missing altogether: no hashes either.
    let f = fixture("v0.7.1");
    f.server
        .add(Route::json("GET", &format!("/dl/v0.7.1/{CHECKSUMS_NAME}"), "nope").status(404));
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    let UpdateStatus::Available {
        verified, assets, ..
    } = status
    else {
        panic!("expected an update");
    };
    assert!(!verified && assets.iter().all(|a| a.sha256.is_none()));
}

#[test]
fn up_to_date_and_channels() {
    let f = fixture("v0.7.1");
    let mut state = UpdateState::default();
    assert_eq!(
        check_for_update_with("0.7.1", Channel::Stable, &f.opts, &mut state),
        UpdateStatus::UpToDate
    );
    let status = check_for_update_with("0.7.1", Channel::Preview, &f.opts, &mut state);
    assert!(matches!(
        status,
        UpdateStatus::Available { ref version, prerelease: true, .. } if version == "0.8.0-beta.1"
    ));
    // Up to date means no extra downloads.
    assert!(f
        .server
        .requests()
        .iter()
        .all(|r| !r.target.starts_with("/dl/v0.7.1")));
    let bad = check_for_update_with("garbage", Channel::Stable, &f.opts, &mut state);
    assert!(matches!(
        bad,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::InvalidVersion(_)
        }
    ));
}

#[test]
fn etag_makes_the_second_check_a_304() {
    let f = fixture("v0.7.1");
    let mut state = UpdateState::default();
    let first = check_for_update_with("0.7.0", Channel::Stable, &f.opts, &mut state);
    assert!(matches!(first, UpdateStatus::Available { .. }));

    // The server now answers conditional requests with 304 and no body.
    f.server.add(
        Route::json("GET", RELEASES_PATH, "")
            .status(304)
            .header("etag", "\"v1\""),
    );
    let second = check_for_update_with("0.7.0", Channel::Stable, &f.opts, &mut state);
    assert_eq!(second, first, "answered from the cached release list");
    let list_requests: Vec<_> = f
        .server
        .requests()
        .into_iter()
        .filter(|r| r.target.starts_with(RELEASES_PATH))
        .collect();
    assert_eq!(list_requests.len(), 2);
    assert_eq!(list_requests[1].header("if-none-match"), Some("\"v1\""));
    // After updating, the same cache says "up to date" without any new request body.
    assert_eq!(
        check_for_update_with("0.7.1", Channel::Stable, &f.opts, &mut state),
        UpdateStatus::UpToDate
    );
    assert_eq!(state.etag.as_deref(), Some("\"v1\""), "etag survives a 304");
}

#[test]
fn a_304_without_cache_is_a_quiet_failure() {
    let f = fixture("v0.7.1");
    f.server
        .add(Route::json("GET", RELEASES_PATH, "").status(304));
    let mut state = UpdateState::default();
    let status = check_for_update_with("0.7.0", Channel::Stable, &f.opts, &mut state);
    assert!(matches!(
        status,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::BadResponse(_)
        }
    ));
    // A stale etag without a cached list is not sent either.
    state.etag = Some("\"old\"".into());
    let _ = check_for_update_with("0.7.0", Channel::Stable, &f.opts, &mut state);
    assert!(f
        .server
        .requests()
        .iter()
        .all(|r| r.header("if-none-match").is_none()));
}

#[test]
fn rate_limits_are_quiet_and_counted() {
    let f = fixture("v0.7.1");
    f.server.add(
        Route::json(
            "GET",
            RELEASES_PATH,
            r#"{"message":"API rate limit exceeded"}"#,
        )
        .status(403)
        .header("x-ratelimit-remaining", "0")
        .header("x-ratelimit-reset", "1893456000"),
    );
    let mut state = UpdateState::default();
    let status = check_for_update_with("0.7.0", Channel::Stable, &f.opts, &mut state);
    assert_eq!(
        status,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::RateLimited {
                retry_at: Some(1_893_456_000)
            }
        }
    );
    assert!(
        state.last_checked.is_some(),
        "a limited request still counts as a check"
    );

    let f = fixture("v0.7.1");
    f.server.add(
        Route::json("GET", RELEASES_PATH, "{}")
            .status(429)
            .header("retry-after", "120"),
    );
    let before = now_secs();
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    match status {
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::RateLimited { retry_at: Some(t) },
        } => assert!(
            (before + 120..=before + 125).contains(&t),
            "{t} vs {before}"
        ),
        other => panic!("unexpected {other:?}"),
    }

    // A plain 403 or a 500 is an HTTP error, not a rate limit.
    let f = fixture("v0.7.1");
    f.server
        .add(Route::json("GET", RELEASES_PATH, "{}").status(403));
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    assert_eq!(
        status,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::Http(403)
        }
    );
    f.server
        .add(Route::json("GET", RELEASES_PATH, "{}").status(500));
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    assert_eq!(
        status,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::Http(500)
        }
    );

    // Garbage bodies are a quiet failure too.
    f.server
        .add(Route::json("GET", RELEASES_PATH, "<html>oops</html>"));
    let status = check_for_update_with(
        "0.7.0",
        Channel::Stable,
        &f.opts,
        &mut UpdateState::default(),
    );
    assert!(matches!(
        status,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::BadResponse(_)
        }
    ));
}

#[test]
fn network_failure_is_quiet_and_not_counted() {
    // Nothing listens on this loopback port.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let opts = UpdateOptions {
        api_base: format!("http://127.0.0.1:{port}"),
        repo: RELEASES_REPO.to_string(),
        public_key: None,
        platform: deb_x64(),
        install_kind: InstallKind::Unknown,
    };
    let mut state = UpdateState::default();
    let status = check_for_update_with("0.7.0", Channel::Stable, &opts, &mut state);
    assert!(matches!(
        status,
        UpdateStatus::CouldNotCheck {
            reason: CheckFailure::Network(_)
        }
    ));
    assert_eq!(state.last_checked, None);
}

#[test]
fn downloads_stay_on_github_or_the_api_origin() {
    let api = "http://127.0.0.1:4000";
    assert!(download_allowed(
        "https://github.com/meetdhruva/lector/releases/download/v1/x",
        api
    ));
    assert!(download_allowed("http://127.0.0.1:4000/dl/x", api));
    assert!(!download_allowed(
        "https://evil.example/SHA256SUMS.txt",
        api
    ));
    assert!(
        !download_allowed("http://github.com/x", api),
        "plain http is refused"
    );
    assert!(!download_allowed("https://github.com.evil.example/x", api));
}

#[test]
fn state_round_trips_and_tolerates_other_versions() {
    let state = UpdateState {
        last_checked: Some(1_700_000_000),
        etag: Some("W/\"abc\"".into()),
        releases: releases(),
    };
    let text = serde_json::to_string(&state).expect("serialize");
    assert_eq!(
        serde_json::from_str::<UpdateState>(&text).expect("parse"),
        state
    );
    assert!(serde_json::from_str::<UpdateState>("{ not json").is_err());
    let newer: UpdateState =
        serde_json::from_str(r#"{"last_checked": 5, "future_field": true}"#).expect("parse");
    assert_eq!(newer.last_checked, Some(5));
    assert_eq!(newer.etag, None);
}
