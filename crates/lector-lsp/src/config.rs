use std::path::{Path, PathBuf};

/// How to launch one language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Display / override name, e.g. `rust-analyzer`.
    pub name: String,
    /// Executable name or path.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// `Language::name` values this server handles.
    pub languages: Vec<String>,
    /// Files that mark a project root (informational; the manager's root is used).
    pub root_markers: Vec<String>,
}

fn cfg(name: &str, command: &str, args: &[&str], langs: &[&str], markers: &[&str]) -> ServerConfig {
    let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect();
    ServerConfig {
        name: name.into(),
        command: command.into(),
        args: v(args),
        languages: v(langs),
        root_markers: v(markers),
    }
}

/// Servers Lector knows about, in priority order (first installed match wins).
pub fn builtin_servers() -> Vec<ServerConfig> {
    vec![
        cfg(
            "rust-analyzer",
            "rust-analyzer",
            &[],
            &["Rust"],
            &["Cargo.toml", "rust-project.json"],
        ),
        cfg(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &["TypeScript", "TSX", "JavaScript"],
            &["tsconfig.json", "jsconfig.json", "package.json"],
        ),
        cfg(
            "pyright",
            "pyright-langserver",
            &["--stdio"],
            &["Python"],
            &[
                "pyproject.toml",
                "setup.py",
                "requirements.txt",
                "pyrightconfig.json",
            ],
        ),
        cfg(
            "pylsp",
            "pylsp",
            &[],
            &["Python"],
            &["pyproject.toml", "setup.py", "requirements.txt"],
        ),
        cfg("gopls", "gopls", &[], &["Go"], &["go.work", "go.mod"]),
        cfg(
            "clangd",
            "clangd",
            // The background index is stored in `<project>/.cache/clangd`.
            &["--background-index=false"],
            &["C", "C++"],
            &[
                "compile_commands.json",
                "compile_flags.txt",
                ".clangd",
                "CMakeLists.txt",
            ],
        ),
        cfg(
            "jdtls",
            "jdtls",
            &[],
            &["Java"],
            &["pom.xml", "build.gradle", "build.gradle.kts"],
        ),
        cfg(
            "kotlin-language-server",
            "kotlin-language-server",
            &[],
            &["Kotlin"],
            &[
                "settings.gradle",
                "settings.gradle.kts",
                "build.gradle",
                "pom.xml",
            ],
        ),
        cfg(
            "sourcekit-lsp",
            "sourcekit-lsp",
            &[],
            &["Swift"],
            &["Package.swift"],
        ),
        cfg("csharp-ls", "csharp-ls", &[], &["C#"], &[".sln", ".csproj"]),
        cfg(
            "solargraph",
            "solargraph",
            &["stdio"],
            &["Ruby"],
            &["Gemfile", ".solargraph.yml"],
        ),
        cfg(
            "intelephense",
            "intelephense",
            &["--stdio"],
            &["PHP"],
            &["composer.json"],
        ),
        cfg(
            "bash-language-server",
            "bash-language-server",
            &["start"],
            &["Bash"],
            &[],
        ),
        cfg(
            "vscode-html-language-server",
            "vscode-html-language-server",
            &["--stdio"],
            &["HTML"],
            &["package.json"],
        ),
        cfg(
            "vscode-css-language-server",
            "vscode-css-language-server",
            &["--stdio"],
            &["CSS"],
            &["package.json"],
        ),
        cfg(
            "vscode-json-language-server",
            "vscode-json-language-server",
            &["--stdio"],
            &["JSON"],
            &["package.json"],
        ),
        cfg(
            "yaml-language-server",
            "yaml-language-server",
            &["--stdio"],
            &["YAML"],
            &[],
        ),
        cfg("taplo", "taplo", &["lsp", "stdio"], &["TOML"], &[]),
        cfg(
            "marksman",
            "marksman",
            &["server"],
            &["Markdown"],
            &[".marksman.toml"],
        ),
    ]
}

/// Built-in servers that build or index the project into its own directory as
/// part of normal operation (jdtls: `.project`/`.classpath`/`.settings`; Gradle's
/// `.gradle/`; SwiftPM's `.build/`; MSBuild's `obj/`). Lector v0.1 is read-only, so
/// callers should only start these when the user enables them explicitly.
pub const WRITES_TO_PROJECT: &[&str] = &[
    "jdtls",
    "kotlin-language-server",
    "sourcekit-lsp",
    "csharp-ls",
];

/// Whether the built-in server `name` writes into the project (see [`WRITES_TO_PROJECT`]).
pub fn writes_to_project(name: &str) -> bool {
    WRITES_TO_PROJECT.contains(&name)
}

/// Settings that keep the built-in server `name` from writing into the project,
/// sent as `initializationOptions` and as the answer to `workspace/configuration`
/// for the section `name`. rust-analyzer: no dependency resolution (`cargo metadata`
/// would create or update `Cargo.lock`), no build scripts or proc macros and no
/// `cargo check` (they build into `target/`).
pub fn read_only_settings(name: &str) -> Option<serde_json::Value> {
    match name {
        "rust-analyzer" => Some(serde_json::json!({
            "cargo": { "noDeps": true, "buildScripts": { "enable": false } },
            "procMacro": { "enable": false },
            "checkOnSave": false,
            "cachePriming": { "enable": false },
        })),
        _ => None,
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn candidates(dir: &Path, cmd: &str) -> Vec<PathBuf> {
    let mut v = vec![dir.join(cmd)];
    if cfg!(windows) && Path::new(cmd).extension().is_none() {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            v.push(dir.join(format!("{cmd}{ext}")));
        }
    }
    v
}

/// Find an executable like the shell would (PATH lookup, PATHEXT on Windows).
/// Commands containing a path separator are checked directly.
pub fn which(command: &str) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    let p = Path::new(command);
    if p.components().count() > 1 || p.is_absolute() {
        return candidates(Path::new(""), command)
            .into_iter()
            .find(|c| is_executable(c));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| candidates(&dir, command))
        .find(|c| is_executable(c))
}

/// LSP `languageId` for a Lector language name.
pub(crate) fn language_id(language: &str) -> String {
    match language {
        "TSX" => "typescriptreact",
        "C++" => "cpp",
        "C#" => "csharp",
        "Bash" => "shellscript",
        other => return other.to_ascii_lowercase(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_server_settings() {
        let ra = read_only_settings("rust-analyzer").expect("rust-analyzer settings");
        assert_eq!(ra["cargo"]["noDeps"], true);
        assert_eq!(ra["cargo"]["buildScripts"]["enable"], false);
        assert_eq!(ra["checkOnSave"], false);
        assert!(read_only_settings("gopls").is_none());
        let clangd = builtin_servers()
            .into_iter()
            .find(|c| c.name == "clangd")
            .expect("clangd");
        assert!(clangd
            .args
            .contains(&"--background-index=false".to_string()));
        for name in WRITES_TO_PROJECT {
            assert!(builtin_servers().iter().any(|c| c.name == *name), "{name}");
        }
        assert!(writes_to_project("jdtls") && !writes_to_project("rust-analyzer"));
    }

    #[test]
    fn builtins_cover_languages() {
        let b = builtin_servers();
        for lang in [
            "Rust",
            "TypeScript",
            "TSX",
            "JavaScript",
            "Python",
            "Go",
            "Java",
            "Kotlin",
            "Swift",
            "C",
            "C++",
            "C#",
            "Ruby",
            "PHP",
            "Bash",
            "HTML",
            "CSS",
            "JSON",
            "YAML",
            "TOML",
            "Markdown",
        ] {
            assert!(
                b.iter().any(|s| s.languages.iter().any(|l| l == lang)),
                "{lang}"
            );
        }
        let py: Vec<_> = b
            .iter()
            .filter(|s| s.languages.contains(&"Python".to_string()))
            .collect();
        assert_eq!(py[0].name, "pyright");
        assert_eq!(py[1].name, "pylsp");
    }

    #[test]
    fn which_finds_sh_and_misses_garbage() {
        if cfg!(unix) {
            assert!(which("sh").is_some());
        }
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
        assert!(which("").is_none());
    }

    #[test]
    fn ids() {
        assert_eq!(language_id("TSX"), "typescriptreact");
        assert_eq!(language_id("Rust"), "rust");
    }
}
