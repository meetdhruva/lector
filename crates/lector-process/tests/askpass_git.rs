//! A real `git` asks for credentials through the askpass server.
//!
//! `harness = false`: this binary is both the test runner and the askpass helper that git
//! runs (`GIT_ASKPASS` = the server's helper program, which runs this executable with the
//! hidden `--lector-askpass-helper` flag in front of the prompt on Unix), exactly like the
//! Lector binaries dispatch the helper mode first thing in `main`.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use lector_process::askpass::{
    askpass_prompt_from_args, run_askpass_client, AskpassServer, PromptKind, PromptReply,
    PromptRequest, Secret,
};
use lector_process::{ProcessCommand, ProcessKind, RunOptions, StdinMode, Supervisor};

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some(prompt) = askpass_prompt_from_args(&args) {
        std::process::exit(run_askpass_client(&prompt));
    }
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("git not installed; skipping askpass_git");
        return;
    }
    git_credential_fill_uses_askpass();
    cancelling_fails_git_without_a_terminal_prompt();
    println!("askpass_git: 2 passed");
}

/// `git credential fill` with an empty config and no helper: git asks `GIT_ASKPASS` for
/// the username and the password.
fn credential_fill(server: &AskpassServer, home: &std::path::Path) -> lector_process::RunOutput {
    let exe = std::env::current_exe().expect("current exe");
    let empty = home.join("empty.gitconfig");
    std::fs::write(&empty, "").expect("write config");
    let cmd = ProcessCommand::new("git")
        .args(["credential", "fill"])
        .cwd(home)
        .env("GIT_CONFIG_GLOBAL", &empty)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", home)
        .envs(server.env(&helper_for(server, &exe)))
        .stdin(StdinMode::Bytes(
            b"protocol=https\nhost=example.invalid\n\n".to_vec(),
        ))
        .kind(ProcessKind::Git)
        .label("git credential fill");
    Supervisor::new()
        .run(&cmd, &RunOptions::default())
        .expect("run git")
}

/// The helper program git runs: the generated shim on Unix. On Windows the hard link would
/// be a copy of this test binary named `lector-askpass.exe`, selected by its name.
fn helper_for(server: &AskpassServer, exe: &std::path::Path) -> std::path::PathBuf {
    server.helper_program(exe).expect("helper program")
}

fn git_credential_fill_uses_askpass() {
    let home = tempfile::tempdir().expect("tempdir");
    let seen: Arc<Mutex<Vec<PromptRequest>>> = Arc::default();
    let s = seen.clone();
    let server = AskpassServer::start(home.path(), move |req| {
        let reply = match req.kind {
            PromptKind::Username => PromptReply::Answer(Secret::new("ann")),
            PromptKind::Password => PromptReply::Answer(Secret::new("s3cret pass")),
            _ => PromptReply::Cancel,
        };
        s.lock().expect("lock").push(req);
        reply
    })
    .expect("server");
    let out = credential_fill(&server, home.path());
    assert!(out.success(), "git failed: {}", out.stderr_text());
    let stdout = out.stdout_text();
    assert!(stdout.contains("username=ann\n"), "{stdout}");
    assert!(stdout.contains("password=s3cret pass\n"), "{stdout}");
    let seen = seen.lock().expect("lock");
    let kinds: Vec<PromptKind> = seen.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, vec![PromptKind::Username, PromptKind::Password]);
    assert!(seen[0].text.contains("example.invalid"), "{:?}", seen[0]);
    println!("git_credential_fill_uses_askpass ... ok");
}

fn cancelling_fails_git_without_a_terminal_prompt() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = AskpassServer::start(home.path(), |_| PromptReply::Cancel).expect("server");
    let out = credential_fill(&server, home.path());
    assert!(!out.success(), "git must fail when the user cancels");
    assert!(!out.stdout_text().contains("password="));
    println!("cancelling_fails_git_without_a_terminal_prompt ... ok");
}
