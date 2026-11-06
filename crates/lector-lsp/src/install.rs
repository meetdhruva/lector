//! Zero-setup language servers: when a server is not on `PATH`, tell the user the exact command
//! that installs it on their operating system. A data table keyed by [`ServerConfig::name`](crate::ServerConfig);
//! Lector never runs these commands itself, it only shows them (with a copy button).

/// An operating system family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    /// macOS.
    MacOs,
    /// Linux and other Unix systems.
    Linux,
    /// Windows.
    Windows,
}

impl Os {
    /// The operating system Lector runs on.
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// One way to install a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallOption {
    /// Short description ("with rustup", "with Homebrew").
    pub label: &'static str,
    /// The command to paste into a terminal.
    pub command: &'static str,
}

/// Where an [`InstallOption`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Only {
    Any,
    Mac,
    Linux,
    Windows,
}

impl Only {
    fn matches(self, os: Os) -> bool {
        matches!(
            (self, os),
            (Only::Any, _)
                | (Only::Mac, Os::MacOs)
                | (Only::Linux, Os::Linux)
                | (Only::Windows, Os::Windows)
        )
    }
}

/// How to install one language server.
#[derive(Debug, Clone, Copy)]
pub struct InstallHint {
    /// `ServerConfig::name`.
    pub server: &'static str,
    /// What it serves ("Rust").
    pub language: &'static str,
    options: &'static [(Only, InstallOption)],
}

const fn opt(only: Only, label: &'static str, command: &'static str) -> (Only, InstallOption) {
    (only, InstallOption { label, command })
}

static HINTS: &[InstallHint] = &[
    InstallHint {
        server: "rust-analyzer",
        language: "Rust",
        options: &[opt(
            Only::Any,
            "with rustup",
            "rustup component add rust-analyzer",
        )],
    },
    InstallHint {
        server: "typescript-language-server",
        language: "TypeScript and JavaScript",
        options: &[opt(
            Only::Any,
            "with npm",
            "npm i -g typescript-language-server typescript",
        )],
    },
    InstallHint {
        server: "pyright",
        language: "Python",
        options: &[
            opt(Only::Any, "with npm", "npm i -g pyright"),
            opt(Only::Any, "with pipx", "pipx install pyright"),
        ],
    },
    InstallHint {
        server: "pylsp",
        language: "Python",
        options: &[opt(
            Only::Any,
            "with pipx",
            "pipx install python-lsp-server",
        )],
    },
    InstallHint {
        server: "gopls",
        language: "Go",
        options: &[opt(
            Only::Any,
            "with the Go toolchain",
            "go install golang.org/x/tools/gopls@latest",
        )],
    },
    InstallHint {
        server: "clangd",
        language: "C and C++",
        options: &[
            opt(Only::Mac, "with Homebrew", "brew install llvm"),
            opt(Only::Linux, "on Debian / Ubuntu", "sudo apt install clangd"),
            opt(
                Only::Linux,
                "on Fedora",
                "sudo dnf install clang-tools-extra",
            ),
            opt(Only::Linux, "on Arch", "sudo pacman -S clang"),
            opt(Only::Windows, "with winget", "winget install LLVM.LLVM"),
        ],
    },
    InstallHint {
        server: "jdtls",
        language: "Java",
        options: &[
            opt(Only::Mac, "with Homebrew", "brew install jdtls"),
            opt(Only::Linux, "on Arch", "yay -S jdtls"),
            opt(Only::Windows, "with Scoop", "scoop install jdtls"),
        ],
    },
    InstallHint {
        server: "kotlin-language-server",
        language: "Kotlin",
        options: &[
            opt(
                Only::Mac,
                "with Homebrew",
                "brew install kotlin-language-server",
            ),
            opt(
                Only::Windows,
                "with Scoop",
                "scoop install kotlin-language-server",
            ),
        ],
    },
    InstallHint {
        server: "sourcekit-lsp",
        language: "Swift",
        options: &[
            opt(Only::Mac, "ships with Xcode", "xcode-select --install"),
            opt(
                Only::Linux,
                "ships with the Swift toolchain",
                "swiftly install latest",
            ),
        ],
    },
    InstallHint {
        server: "csharp-ls",
        language: "C#",
        options: &[opt(
            Only::Any,
            "with the .NET SDK",
            "dotnet tool install --global csharp-ls",
        )],
    },
    InstallHint {
        server: "solargraph",
        language: "Ruby",
        options: &[opt(Only::Any, "with RubyGems", "gem install solargraph")],
    },
    InstallHint {
        server: "intelephense",
        language: "PHP",
        options: &[opt(Only::Any, "with npm", "npm i -g intelephense")],
    },
    InstallHint {
        server: "bash-language-server",
        language: "Shell",
        options: &[opt(Only::Any, "with npm", "npm i -g bash-language-server")],
    },
    InstallHint {
        server: "vscode-html-language-server",
        language: "HTML",
        options: &[opt(
            Only::Any,
            "with npm",
            "npm i -g vscode-langservers-extracted",
        )],
    },
    InstallHint {
        server: "vscode-css-language-server",
        language: "CSS",
        options: &[opt(
            Only::Any,
            "with npm",
            "npm i -g vscode-langservers-extracted",
        )],
    },
    InstallHint {
        server: "vscode-json-language-server",
        language: "JSON",
        options: &[opt(
            Only::Any,
            "with npm",
            "npm i -g vscode-langservers-extracted",
        )],
    },
    InstallHint {
        server: "yaml-language-server",
        language: "YAML",
        options: &[opt(Only::Any, "with npm", "npm i -g yaml-language-server")],
    },
    InstallHint {
        server: "taplo",
        language: "TOML",
        options: &[
            opt(
                Only::Any,
                "with cargo",
                "cargo install taplo-cli --locked --features lsp",
            ),
            opt(Only::Mac, "with Homebrew", "brew install taplo"),
        ],
    },
    InstallHint {
        server: "marksman",
        language: "Markdown",
        options: &[
            opt(Only::Mac, "with Homebrew", "brew install marksman"),
            opt(Only::Windows, "with Scoop", "scoop install marksman"),
            opt(Only::Linux, "with Snap", "sudo snap install marksman"),
        ],
    },
];

impl InstallHint {
    /// The ways to install the server on `os`, best first.
    pub fn options_for(&self, os: Os) -> Vec<InstallOption> {
        self.options
            .iter()
            .filter(|(only, _)| only.matches(os))
            .map(|(_, o)| *o)
            .collect()
    }

    /// The preferred install command on `os`.
    pub fn primary(&self, os: Os) -> Option<InstallOption> {
        self.options_for(os).into_iter().next()
    }
}

/// The install hint of the built-in server `server` ([`crate::ServerConfig::name`]).
pub fn install_hint(server: &str) -> Option<&'static InstallHint> {
    HINTS.iter().find(|h| h.server == server)
}

/// Every hint in the table.
pub fn install_hints() -> &'static [InstallHint] {
    HINTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_server_has_a_hint_on_some_os() {
        for server in crate::builtin_servers() {
            let hint = install_hint(&server.name)
                .unwrap_or_else(|| panic!("no install hint for {}", server.name));
            assert!(
                [Os::MacOs, Os::Linux, Os::Windows]
                    .iter()
                    .any(|os| hint.primary(*os).is_some()),
                "{} has no command at all",
                server.name
            );
        }
    }

    #[test]
    fn hints_name_known_servers_and_have_commands() {
        let names: Vec<String> = crate::builtin_servers()
            .into_iter()
            .map(|s| s.name)
            .collect();
        for hint in install_hints() {
            assert!(names.contains(&hint.server.to_string()), "{}", hint.server);
        }
        let ra = install_hint("rust-analyzer").expect("rust-analyzer");
        for os in [Os::MacOs, Os::Linux, Os::Windows] {
            assert_eq!(
                ra.primary(os).map(|o| o.command),
                Some("rustup component add rust-analyzer")
            );
        }
    }

    #[test]
    fn commands_are_os_specific_where_needed() {
        let clangd = install_hint("clangd").expect("clangd");
        assert_eq!(
            clangd.primary(Os::MacOs).map(|o| o.command),
            Some("brew install llvm")
        );
        assert_eq!(
            clangd.primary(Os::Linux).map(|o| o.command),
            Some("sudo apt install clangd")
        );
        assert_eq!(clangd.options_for(Os::Linux).len(), 3);
        assert_eq!(
            clangd.primary(Os::Windows).map(|o| o.command),
            Some("winget install LLVM.LLVM")
        );
        let pyright = install_hint("pyright").expect("pyright");
        assert_eq!(pyright.options_for(Os::Linux).len(), 2);
        assert_eq!(
            pyright.options_for(Os::Linux)[1].command,
            "pipx install pyright"
        );
        assert_eq!(
            install_hint("typescript-language-server")
                .and_then(|h| h.primary(Os::Linux))
                .map(|o| o.command),
            Some("npm i -g typescript-language-server typescript")
        );
        assert_eq!(
            install_hint("gopls")
                .and_then(|h| h.primary(Os::MacOs))
                .map(|o| o.command),
            Some("go install golang.org/x/tools/gopls@latest")
        );
        assert!(install_hint("unknown-server").is_none());
    }
}
