//! The terminal's default scrub list covers every askpass variable `lector-process` adds
//! to git commands (the two lists live in crates that do not depend on each other).

use lector_terminal::shell::{env_is_scrubbed, DEFAULT_ENV_SCRUB};

#[test]
fn default_scrub_covers_askpass_variables() {
    let patterns: Vec<String> = DEFAULT_ENV_SCRUB.iter().map(|s| (*s).to_owned()).collect();
    for name in lector_process::askpass::SCRUB_ENV {
        assert!(
            env_is_scrubbed(name, &patterns),
            "{name} reaches terminal shells"
        );
    }
}
