//! Tests of the sync model: states, plans, failures, outcomes, branch names
//! and credential prompts.

use super::*;

fn status(ahead: u32, behind: u32) -> SyncStatus {
    SyncStatus {
        loaded: true,
        branch: Some("main".into()),
        head_short: Some("abc1234".into()),
        upstream: Some("origin/main".into()),
        push_remote: Some("origin".into()),
        ahead,
        behind,
        remotes: vec![RemoteInfo {
            name: "origin".into(),
            url: "https://example.com/r.git".into(),
        }],
        ..Default::default()
    }
}

#[test]
fn every_state_maps_to_a_label_and_an_action() {
    let mut no_upstream = status(0, 0);
    no_upstream.upstream = None;
    let mut gone = no_upstream.clone();
    gone.upstream_gone = true;
    let mut detached = status(0, 0);
    detached.detached = true;
    detached.branch = None;
    let mut no_remote = no_upstream.clone();
    no_remote.remotes.clear();
    no_remote.push_remote = None;
    let cases: Vec<(SyncStatus, SyncState, &str, &str, PrimaryAction)> = vec![
        (
            SyncStatus::default(),
            SyncState::Loading,
            "",
            "",
            PrimaryAction::None,
        ),
        (
            status(0, 0),
            SyncState::UpToDate,
            "up to date",
            "",
            PrimaryAction::Sync(SyncAction::Fetch),
        ),
        (
            status(2, 0),
            SyncState::NeedsPush,
            "push",
            "↑2",
            PrimaryAction::Sync(SyncAction::Push),
        ),
        (
            status(0, 3),
            SyncState::NeedsPull,
            "pull",
            "↓3",
            PrimaryAction::Sync(SyncAction::Pull),
        ),
        (
            status(2, 1),
            SyncState::Diverged,
            "diverged",
            "↑2 ↓1",
            PrimaryAction::Sync(SyncAction::PullRebase),
        ),
        (
            no_upstream,
            SyncState::NoUpstream,
            "publish",
            "",
            PrimaryAction::Sync(SyncAction::Publish),
        ),
        (
            gone,
            SyncState::UpstreamGone,
            "upstream gone",
            "",
            PrimaryAction::Sync(SyncAction::Publish),
        ),
        (
            detached,
            SyncState::Detached,
            "detached",
            "",
            PrimaryAction::NewBranch,
        ),
        (
            no_remote,
            SyncState::NoRemote,
            "no remote",
            "",
            PrimaryAction::None,
        ),
    ];
    for (st, state, word, counts, primary) in cases {
        assert_eq!(st.state(), state);
        let v = control_view(&st);
        assert_eq!(v.state_word, word, "{state:?}");
        assert_eq!(v.counts, counts, "{state:?}");
        assert_eq!(v.primary, primary, "{state:?}");
        assert!(!v.tooltip.is_empty(), "{state:?}");
    }
    assert_eq!(control_view(&status(0, 0)).branch, "main");
    let mut d = status(0, 0);
    d.detached = true;
    assert_eq!(control_view(&d).branch, "detached abc1234");
    assert_eq!(
        PrimaryAction::Sync(SyncAction::Push).action_name(),
        Some("git::Push")
    );
    assert_eq!(
        PrimaryAction::NewBranch.action_name(),
        Some("git::NewBranch")
    );
    assert_eq!(PrimaryAction::None.action_name(), None);
}

#[test]
fn actions_map_to_commands_and_never_plain_force() {
    for a in SyncAction::ALL {
        let (id, args) = a.command();
        assert!(id.starts_with("git."));
        assert!(args.get("force").is_none(), "no plain --force");
    }
    assert_eq!(
        SyncAction::Pull.command(),
        ("git.pull", json!({"mode": "ff_only"}))
    );
    assert_eq!(
        SyncAction::PullRebase.command(),
        ("git.pull", json!({"mode": "rebase"}))
    );
    assert_eq!(
        SyncAction::ForcePushWithLease.command(),
        ("git.push", json!({"force_with_lease": true}))
    );
    assert_eq!(
        SyncAction::Publish.command(),
        ("git.push", json!({"set_upstream": true}))
    );
}

#[test]
fn force_with_lease_is_always_confirmed_first() {
    for st in [status(0, 0), status(3, 0), status(1, 4)] {
        match plan(SyncAction::ForcePushWithLease, &st, None) {
            Plan::Confirm(confirm, cmds) => {
                assert_eq!(confirm.title, "Force push to origin/main?");
                assert!(confirm.message[0].contains("origin/main"));
                assert!(confirm.message[0].contains("may be discarded"));
                assert!(confirm.message[1].contains("--force-with-lease"));
                assert_eq!(cmds, vec![SyncAction::ForcePushWithLease.command()]);
            }
            other => panic!("force push must be confirmed, got {other:?}"),
        }
    }
    // Other actions run directly; a busy repository blocks everything.
    assert!(matches!(
        plan(SyncAction::Push, &status(1, 0), None),
        Plan::Run(_)
    ));
    assert!(matches!(
        plan(
            SyncAction::ForcePushWithLease,
            &status(1, 0),
            Some("Fetching")
        ),
        Plan::Blocked(_)
    ));
    let mut detached = status(0, 0);
    detached.detached = true;
    assert!(matches!(
        plan(SyncAction::Push, &detached, None),
        Plan::Blocked(_)
    ));
    assert!(matches!(
        plan(SyncAction::Fetch, &detached, None),
        Plan::Run(_)
    ));
    let mut no_up = status(0, 0);
    no_up.upstream = None;
    assert!(matches!(
        plan(SyncAction::Pull, &no_up, None),
        Plan::Blocked(_)
    ));
}

fn err(kind: SyncErrorKind) -> SyncError {
    let mut e = SyncError::new(kind, "git said no");
    if kind == SyncErrorKind::HookDeclined {
        e.hook_output = Some("pre-push: tests failed".into());
    }
    if kind == SyncErrorKind::LocalChanges {
        e.paths = vec!["a.txt".into()];
    }
    e
}

#[test]
fn every_failure_kind_maps_to_a_message_and_next_steps() {
    use NextStep as S;
    use SyncErrorKind as K;
    let push = OpKind::Sync(SyncAction::Push);
    let pull = OpKind::Sync(SyncAction::Pull);
    let rebase = OpKind::Sync(SyncAction::PullRebase);
    let switch = OpKind::Switch("feature".into());
    let force = OpKind::Sync(SyncAction::ForcePushWithLease);
    let delete = OpKind::DeleteBranch("x".into());
    let create = OpKind::CreateBranch("x".into());
    let ctx = FailureContext {
        remote_url: Some("https://example.com/r.git".into()),
        credential_helpers: vec!["store".into()],
        prompt_answered: Some(false),
        ssh_agent: false,
    };
    let cases: Vec<(K, &OpKind, Vec<S>)> = vec![
        (K::NoUpstream, &pull, vec![S::Publish, S::SetUpstream]),
        (K::NoRemote, &push, vec![]),
        (K::DetachedHead, &push, vec![S::NewBranch]),
        (K::Rejected, &push, vec![S::PullFirst, S::PullRebase]),
        (K::StaleLease, &force, vec![S::Fetch]),
        (K::AuthenticationFailed, &push, vec![S::Retry]),
        (K::HostKeyUnknown, &push, vec![S::Retry]),
        (K::HostKeyChanged, &push, vec![]),
        (K::NetworkUnreachable, &pull, vec![S::Retry]),
        (K::RemoteNotFound, &push, vec![S::Retry]),
        (K::HookDeclined, &push, vec![S::Retry]),
        (K::Diverged, &pull, vec![S::PullRebase]),
        (
            K::LocalChanges,
            &pull,
            vec![S::StashAndPull(SyncAction::Pull)],
        ),
        (
            K::LocalChanges,
            &switch,
            vec![S::StashAndSwitch("feature".into())],
        ),
        (
            K::DirtyWorktree,
            &rebase,
            vec![S::StashAndPull(SyncAction::PullRebase)],
        ),
        (K::NotMerged, &delete, vec![]),
        (K::InvalidName, &create, vec![]),
        (K::AlreadyExists, &create, vec![]),
        (K::NotFound, &switch, vec![]),
        (K::RefChanged, &pull, vec![]),
        (K::Other, &push, vec![S::Retry]),
    ];
    for (kind, op, steps) in cases {
        let v = failure_view(op, &err(kind), &ctx);
        assert_eq!(v.kind, Some(kind));
        assert_eq!(v.steps, steps, "{kind:?} after {op:?}");
        assert!(v.title.ends_with("failed"), "{kind:?}");
        assert!(!v.message.is_empty(), "{kind:?}");
        assert!(v.details.is_some(), "{kind:?} shows git's output");
    }
    let hook = failure_view(&push, &err(K::HookDeclined), &ctx);
    assert!(hook.details_are_hook_output);
    assert_eq!(hook.details.as_deref(), Some("pre-push: tests failed"));
    let auth = failure_view(&push, &err(K::AuthenticationFailed), &ctx);
    assert!(auth.message[1].contains("credential helper first (store)"));
    assert!(auth.message[1].contains("cancelled"));
    let local = failure_view(&switch, &err(K::LocalChanges), &ctx);
    assert_eq!(local.paths, vec!["a.txt".to_string()]);
    let changed = failure_view(&push, &err(K::HostKeyChanged), &ctx);
    assert!(changed.message[0].starts_with("WARNING"));
    assert_eq!(generic_failure(&push, "boom").steps, vec![S::Retry]);
}

#[test]
fn credential_sources_name_ssh_and_https_paths() {
    let ssh = credential_sources(&FailureContext {
        remote_url: Some("git@github.com:me/r.git".into()),
        ssh_agent: true,
        ..Default::default()
    });
    assert!(ssh.contains("SSH") && ssh.contains("ssh-agent"));
    let https = credential_sources(&FailureContext {
        remote_url: Some("https://***@example.com/r.git".into()),
        prompt_answered: Some(true),
        ..Default::default()
    });
    assert!(https.contains("No git credential helper"));
    assert!(https.contains("refused that answer"));
}

#[test]
fn outcomes_become_toasts() {
    let pull = json!({"status": "fast_forwarded", "branch": "main", "upstream": "origin/main", "incoming": 3, "rebased": 0, "conflicts": []});
    let v = success_view(SyncAction::Pull, &pull, "x");
    assert_eq!(v.message, "Pulled 3 commits into main");
    assert_eq!(v.pulled, 3);
    let v = success_view(
        SyncAction::Pull,
        &json!({"status": "up_to_date", "incoming": 0}),
        "x",
    );
    assert_eq!(v.message, "Already up to date");
    let v = success_view(
        SyncAction::PullRebase,
        &json!({"status": "conflicts", "conflicts": ["a", "b"]}),
        "x",
    );
    assert!(v.conflicts);
    assert!(v.message.contains("2 files"));
    let push = json!({"branch": "main", "remote": "origin", "remote_branch": "main", "up_to_date": false, "set_upstream": false});
    assert_eq!(
        success_view(SyncAction::Push, &push, "x").message,
        "Pushed main → origin/main"
    );
    let publish = json!({"branch": "f", "remote": "origin", "remote_branch": "f", "up_to_date": false, "set_upstream": true});
    assert_eq!(
        success_view(SyncAction::Publish, &publish, "x").message,
        "Published f → origin/f"
    );
    assert_eq!(
        success_view(SyncAction::Fetch, &json!({"updated": []}), "x").message,
        "Fetched: already up to date"
    );
}

#[test]
fn branch_names_follow_git_ref_rules() {
    let existing = vec!["main".to_string()];
    let ok = [
        "feature",
        "feature/login",
        "fix-123",
        "release/v1.2",
        "ünïcode",
        "a.b",
    ];
    for name in ok {
        assert_eq!(validate_branch_name(name, &existing), Ok(()), "{name}");
    }
    let bad = [
        ("", "Enter"),
        ("main", "already exists"),
        ("has space", "spaces"),
        (" lead", "spaces"),
        ("-x", "start with '-'"),
        ("HEAD", "reserved"),
        ("@", "reserved"),
        ("a..b", "'..'"),
        ("a~1", "'~'"),
        ("a^", "'^'"),
        ("a:b", "':'"),
        ("a?", "'?'"),
        ("a*", "'*'"),
        ("a[b", "'['"),
        ("a\\b", "'\\'"),
        ("a@{b", "'@{'"),
        ("/a", "Slashes"),
        ("a/", "Slashes"),
        ("a//b", "Slashes"),
        ("a.", "end with '.'"),
        (".a", "start with '.'"),
        ("a/.b", "start with '.'"),
        ("a.lock", ".lock"),
        ("a/b.lock/c", ".lock"),
        ("tab\there", "control"),
    ];
    for (name, why) in bad {
        match validate_branch_name(name, &existing) {
            Err(e) => assert!(e.contains(why), "{name:?}: {e}"),
            Ok(()) => panic!("{name:?} must be rejected"),
        }
    }
}

#[test]
fn prompts_are_classified_into_dialogs() {
    let host = "The authenticity of host 'github.com (140.82.121.4)' can't be established.\nED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.\nThis key is not known by any other names.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
    let spec = prompt_spec(host, askpass::classify_prompt(host));
    assert_eq!(spec.kind, PromptDialogKind::HostKey);
    assert!(!spec.masked);
    assert_eq!(spec.host.as_deref(), Some("github.com (140.82.121.4)"));
    assert_eq!(spec.key_type.as_deref(), Some("ED25519"));
    assert_eq!(
        spec.fingerprint.as_deref(),
        Some("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU")
    );
    assert_eq!(HOST_KEY_TRUST_ANSWER, "yes");

    let pass = "Enter passphrase for key '/home/ann/.ssh/id_ed25519': ";
    let spec = prompt_spec(pass, askpass::classify_prompt(pass));
    assert_eq!(spec.kind, PromptDialogKind::Passphrase);
    assert!(spec.masked);
    assert_eq!(spec.key_path.as_deref(), Some("/home/ann/.ssh/id_ed25519"));

    let user = "Username for 'https://github.com': ";
    let spec = prompt_spec(user, askpass::classify_prompt(user));
    assert_eq!(spec.kind, PromptDialogKind::Username);
    assert!(!spec.masked);
    assert_eq!(spec.url.as_deref(), Some("https://github.com"));
    assert_eq!(spec.host.as_deref(), Some("github.com"));

    let pw = "Password for 'https://ann:tok@github.com': ";
    let spec = prompt_spec(pw, askpass::classify_prompt(pw));
    assert_eq!(spec.kind, PromptDialogKind::Password);
    assert!(spec.masked);
    let url = spec.url.unwrap_or_default();
    assert!(!url.contains("tok"), "credentials stripped: {url}");
    assert!(url.contains("github.com"));

    let ssh_pw = "ann@build.example's password: ";
    let spec = prompt_spec(ssh_pw, askpass::classify_prompt(ssh_pw));
    assert_eq!(spec.kind, PromptDialogKind::Password);
    assert_eq!(spec.host.as_deref(), Some("ann@build.example"));

    let otp = "Verification code: ";
    let spec = prompt_spec(otp, PromptKind::Other);
    assert_eq!(spec.kind, PromptDialogKind::Generic);
    assert!(spec.masked, "looks secret");
    assert_eq!(
        spec.prompt,
        otp.trim_end(),
        "shown verbatim (trailing space trimmed)"
    );
    let plain = "Which remote? ";
    assert!(!prompt_spec(plain, PromptKind::Other).masked);
}

#[test]
fn spoofed_prompts_get_the_generic_dialog_without_trusted_fields() {
    let cases = [
            // An SSH server's keyboard-interactive prompt (OpenSSH's `(user@host) ` prefix).
            "(git@evil.example) Enter passphrase for key '/home/u/.ssh/id_ed25519': ",
            "(git@evil.example) The authenticity of host 'github.com (1.2.3.4)' can't be established.\nED25519 key fingerprint is SHA256:x.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
            // The magic words anywhere else.
            "Please confirm (yes/no) to continue connecting: ",
            "Your passphrase: ",
        ];
    for text in cases {
        let kind = askpass::classify_prompt(text);
        let spec = prompt_spec(text, kind);
        assert_eq!(spec.kind, PromptDialogKind::Generic, "{text:?}");
        assert_eq!(spec.title, "The server asks:");
        assert_eq!(
            (
                &spec.host,
                &spec.key_path,
                &spec.fingerprint,
                &spec.key_type,
                &spec.url
            ),
            (&None, &None, &None, &None, &None),
            "{text:?}"
        );
    }
    // A caller may downgrade a template (never upgrade a non-template).
    let pass = "Enter passphrase for key '/k': ";
    assert_eq!(
        prompt_spec(pass, PromptKind::Other).kind,
        PromptDialogKind::Generic
    );
    assert_eq!(
        prompt_spec("Your passphrase: ", PromptKind::Passphrase).kind,
        PromptDialogKind::Generic
    );
    // Control characters and bidi overrides never reach the dialog text.
    let spec = prompt_spec("Code \u{1b}[2J\u{202e}?", PromptKind::Other);
    assert!(!spec.prompt.contains('\u{1b}') && !spec.prompt.contains('\u{202e}'));
}

#[test]
fn prompt_context_checks_key_files_and_hosts() {
    let exists = |p: &str| p == "/home/ann/.ssh/id_ed25519";
    let ctx = PromptContextInfo {
        remote: Some("git@github.com:o/r.git"),
        attributed: true,
        key_exists: &exists,
    };
    let pass = "Enter passphrase for key '/home/ann/.ssh/id_ed25519': ";
    let spec = prompt_spec_in(pass, PromptKind::Passphrase, &ctx);
    assert_eq!(spec.kind, PromptDialogKind::Passphrase);
    assert_eq!(spec.warning, None);
    let missing = "Enter passphrase for key '/home/ann/.ssh/not_there': ";
    let spec = prompt_spec_in(missing, PromptKind::Passphrase, &ctx);
    assert_eq!(spec.kind, PromptDialogKind::Generic);
    assert_eq!(spec.key_path, None);

    let same = "Password for 'https://ann@GitHub.com': ";
    let spec = prompt_spec_in(same, PromptKind::Password, &ctx);
    assert_eq!(spec.warning, None);
    let other = "Password for 'https://ann@evil.example': ";
    let spec = prompt_spec_in(other, PromptKind::Password, &ctx);
    assert_eq!(spec.url.as_deref(), Some("https://***@evil.example"));
    let warning = spec.warning.unwrap_or_default();
    assert!(warning.contains("evil.example") && warning.contains("github.com"));
    let host_key = "The authenticity of host 'evil.example (10.0.0.1)' can't be established.\nED25519 key fingerprint is SHA256:x.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
    let spec = prompt_spec_in(host_key, PromptKind::HostKeyConfirm, &ctx);
    assert!(spec.warning.is_some());
    let ssh_pw = "ann@github.com's password: ";
    assert_eq!(
        prompt_spec_in(ssh_pw, PromptKind::Password, &ctx).warning,
        None
    );

    let unknown = PromptContextInfo {
        remote: None,
        attributed: false,
        key_exists: &exists,
    };
    let spec = prompt_spec_in(
        "Username for 'https://x.example': ",
        PromptKind::Username,
        &unknown,
    );
    assert!(spec.warning.unwrap_or_default().contains("cannot tell"));
}

#[test]
fn last_fetched_is_human() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let ago = |s: u64| Some(now - Duration::from_secs(s));
    assert_eq!(last_fetched_label(None, now), "Never fetched");
    assert_eq!(last_fetched_label(ago(5), now), "Last fetched just now");
    assert_eq!(last_fetched_label(ago(720), now), "Last fetched 12 min ago");
    assert_eq!(
        last_fetched_label(ago(3600), now),
        "Last fetched 1 hour ago"
    );
    assert_eq!(
        last_fetched_label(ago(3 * 86_400), now),
        "Last fetched 3 days ago"
    );
}

#[test]
fn push_target_and_remote_url() {
    let st = status(1, 0);
    assert_eq!(st.push_target().as_deref(), Some("origin/main"));
    assert_eq!(st.remote_url(), Some("https://example.com/r.git"));
    let mut other = st.clone();
    other.upstream = None;
    other.branch = Some("feat".into());
    assert_eq!(other.push_target().as_deref(), Some("origin/feat"));
}
