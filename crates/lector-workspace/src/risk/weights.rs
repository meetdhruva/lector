//! The tunable numbers of the risk model.
//!
//! Every raising signal has one *base weight* (what a single typical instance
//! contributes to the noisy-or score) and every dampening class has the fraction
//! of the score it removes. A signal may scale its base weight by a magnitude
//! (for example more removed error paths, more churn); the weights below are
//! the part that is fitted against the labelled evaluation set
//! (`crates/lector-app/tests/risk_eval`, `examples/risk_fit.rs`).

macro_rules! risk_weights {
    ($($field:ident = $default:expr, $doc:literal;)*) => {
        /// Base weights of all signals; see the module docs. `Default` is the tuned set.
        #[derive(Debug, Clone, PartialEq)]
        pub struct RiskWeights {
            $(#[doc = $doc] pub $field: f32,)*
        }

        impl RiskWeights {
            /// Names of the weights, in [`get`](Self::get) / [`set`](Self::set) order.
            pub const NAMES: &'static [&'static str] = &[$(stringify!($field)),*];

            /// Number of weights.
            pub const COUNT: usize = Self::NAMES.len();

            /// The weight with index `i` (see [`NAMES`](Self::NAMES)); 0 when out of range.
            pub fn get(&self, i: usize) -> f32 {
                let all = [$(self.$field),*];
                all.get(i).copied().unwrap_or(0.0)
            }

            /// Sets the weight with index `i`; ignored when out of range.
            pub fn set(&mut self, i: usize, v: f32) {
                let mut idx = 0usize;
                $(
                    if idx == i {
                        self.$field = v;
                        return;
                    }
                    idx += 1;
                )*
                let _ = idx;
            }

            /// The hand-set weights the fit starts from: the constants of the first version of the model for
    /// the old signals, first guesses for the signals added with the evaluation set.
            pub fn baseline() -> Self {
                RiskWeights { $($field: $default),* }
            }
        }
    };
}

risk_weights! {
    api_removed = 0.50, "An exported item was removed.";
    api_changed = 0.45, "The signature or definition of an exported item changed.";
    api_added = 0.12, "A new exported item.";
    error_handling = 0.28, "Error handling removed (scaled up by the number of paths).";
    conditionals = 0.12, "Conditional branches removed (scaled up by the number of branches).";
    panic_added = 0.20, "`unwrap`/`expect`/`panic!` added.";
    unsafe_code = 0.45, "`unsafe` added.";
    dynamic_exec = 0.40, "Process execution, shell, unsafe deserialization added.";
    dynamic_eval = 0.45, "`eval`-style dynamic code evaluation added.";
    html_injection = 0.30, "Raw HTML injection (`innerHTML`, `document.write`) added.";
    shell_interpolated = 0.45, "A shell command assembled from interpolated strings (or `sh -c`).";
    sql_string = 0.40, "SQL assembled with string formatting.";
    auth_removed = 0.50, "An authentication/authorization check was removed.";
    verification_disabled = 0.50, "Certificate or signature verification disabled.";
    weak_crypto = 0.35, "A weak hash or cipher added, or a hardening primitive removed.";
    hardcoded_secret = 0.45, "A credential literal added.";
    secret_compare = 0.35, "A secret compared with `==` instead of in constant time.";
    migration_destructive = 0.50, "Destructive schema/data migration (DROP, TRUNCATE, DELETE without WHERE).";
    migration_alter = 0.25, "Schema alteration that can fail or lose data on existing rows.";
    concurrency_removed = 0.40, "Locking or awaiting removed.";
    concurrency_weakened = 0.40, "Weaker memory ordering, `unsafe impl Send/Sync`, `static mut`.";
    concurrency_added = 0.15, "New threads, tasks or goroutines.";
    dep_added = 0.25, "A new dependency.";
    dep_script = 0.50, "An install-time script in a package manifest.";
    dep_source = 0.40, "A dependency from git/URL/path, wildcard or loosened pin, or a replace directive.";
    ci_unsafe = 0.45, "A CI change that weakens its safety (`pull_request_target`, write-all, ...).";
    ci_check_removed = 0.40, "A test, lint or scan step removed from CI.";
    container_unsafe = 0.40, "A container change that weakens isolation (root, remote ADD, privileged).";
    test_disabled = 0.35, "A test skipped or ignored.";
    config_flip = 0.30, "A boolean default flipped (scaled up for security-looking keys).";
    path_ci = 0.35, "CI/CD pipeline file.";
    path_container = 0.30, "Container build/runtime file.";
    path_migration = 0.30, "Database migration file.";
    path_secrets = 0.40, "Secrets/credentials file.";
    path_infra = 0.30, "Infrastructure definition.";
    path_auth = 0.30, "Auth/crypto/permissions words in the path.";
    tests_removed = 0.30, "Tests removed or deleted.";
    assertions_removed = 0.20, "Assertions removed from non-test code.";
    source_without_tests = 0.10, "Source changed but no test changed in the changeset.";
    churn = 0.10, "Many lines changed (multiplied by 1 to 3.5 with the size).";
    deleted_file = 0.15, "A source file was deleted.";
    manifest = 0.20, "A dependency or build manifest changed.";
    config = 0.10, "A configuration file changed.";
    lockfile = 0.10, "A lockfile changed.";
    binary = 0.20, "A binary file changed.";
    too_large = 0.25, "Too large to analyze.";
    damp_generated = 0.70, "Dampening of generated or minified files.";
    damp_lockfile = 0.50, "Dampening of lockfile contents.";
    damp_test = 0.35, "Dampening of test files.";
    damp_docs = 0.50, "Dampening of documentation.";
}

impl RiskWeights {
    /// The shipped weights: [`baseline`](Self::baseline) after the deterministic coordinate
    /// search of `crates/lector-app/examples/risk_fit.rs` on the tune split of the evaluation set
    /// (bounded to 0.6x..1.5x of the start values). It moved two weights: `api_added` (new exports
    /// are additive and rarely break callers) and `error_handling` (lost error paths outrank
    /// churn and added API); every other weight kept its hand-set value.
    pub fn tuned() -> Self {
        RiskWeights {
            api_added: 0.07,
            error_handling: 0.42,
            ..Self::baseline()
        }
    }
}

impl Default for RiskWeights {
    fn default() -> Self {
        Self::tuned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_set_round_trip() {
        let mut w = RiskWeights::baseline();
        for i in 0..RiskWeights::COUNT {
            let v = w.get(i);
            assert!((0.0..=1.0).contains(&v), "{}", RiskWeights::NAMES[i]);
            w.set(i, v + 0.01);
            assert!((w.get(i) - v - 0.01).abs() < 1e-6);
        }
        assert_eq!(w.get(RiskWeights::COUNT), 0.0);
        let before = w.clone();
        w.set(RiskWeights::COUNT + 3, 0.9);
        assert_eq!(w, before);
    }

    #[test]
    fn names_are_unique() {
        let mut names = RiskWeights::NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), RiskWeights::COUNT);
    }
}
