//! Violations, per-rule results and the ratchet comparison.

use std::collections::BTreeMap;

/// One finding: `rule  file:line  message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Rule name.
    pub rule: &'static str,
    /// Repository-relative path with `/` separators.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// What is wrong and what to do instead.
    pub message: String,
    /// Baseline key this finding counts towards (the file by default).
    pub key: String,
    /// What this finding adds to its key (1, or the line count for `size`).
    pub weight: u64,
}

impl Violation {
    /// A finding that counts once against its file.
    pub fn new(rule: &'static str, file: &str, line: u32, message: impl Into<String>) -> Violation {
        Violation {
            rule,
            file: file.to_string(),
            line,
            message: message.into(),
            key: file.to_string(),
            weight: 1,
        }
    }

    /// Count this finding against `key` instead of the file.
    pub fn keyed(mut self, key: impl Into<String>) -> Violation {
        self.key = key.into();
        self
    }

    /// Give this finding a weight other than 1.
    pub fn weighted(mut self, weight: u64) -> Violation {
        self.weight = weight;
        self
    }

    /// `rule  file:line  message`.
    pub fn render(&self) -> String {
        format!(
            "{}  {}:{}  {}",
            self.rule, self.file, self.line, self.message
        )
    }
}

/// What the "now" and "baseline" columns count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Sum of the weights (violations).
    Count,
    /// Number of keys; the weights are line counts (`size`).
    Files,
}

/// The result of running one rule.
#[derive(Debug, Clone)]
pub struct RuleOutput {
    /// Rule name.
    pub rule: &'static str,
    /// Findings, sorted by file, line, message.
    pub violations: Vec<Violation>,
    /// Baseline key to count.
    pub entries: BTreeMap<String, u64>,
    /// How totals are computed.
    pub unit: Unit,
    /// A remark printed with the table (for example "crates/lector-review does not exist yet").
    pub notice: Option<String>,
}

impl RuleOutput {
    /// Build the result from findings: sorts them and sums the weights per key.
    pub fn new(rule: &'static str, mut violations: Vec<Violation>, unit: Unit) -> RuleOutput {
        violations
            .sort_by(|a, b| (&a.file, a.line, &a.message).cmp(&(&b.file, b.line, &b.message)));
        let mut entries: BTreeMap<String, u64> = BTreeMap::new();
        for v in &violations {
            *entries.entry(v.key.clone()).or_insert(0) += v.weight;
        }
        RuleOutput {
            rule,
            violations,
            entries,
            unit,
            notice: None,
        }
    }

    /// Attach a notice.
    pub fn with_notice(mut self, notice: impl Into<String>) -> RuleOutput {
        self.notice = Some(notice.into());
        self
    }

    /// The number shown in the "now" column.
    pub fn total(&self) -> u64 {
        total(&self.entries, self.unit)
    }
}

/// Column value for a set of entries.
pub fn total(entries: &BTreeMap<String, u64>, unit: Unit) -> u64 {
    match unit {
        Unit::Count => entries.values().sum(),
        Unit::Files => entries.len() as u64,
    }
}

/// One difference between the current findings and the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Baseline key (file, edge, ...).
    pub key: String,
    /// Count now (0 when the key disappeared).
    pub now: u64,
    /// Count in the baseline (0 when the key is new).
    pub base: u64,
}

/// Result of comparing the current entries with the baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// New keys and keys whose count rose: these fail the check.
    pub raised: Vec<Change>,
    /// Keys whose count fell or that disappeared: the baseline must be lowered.
    pub lowered: Vec<Change>,
}

/// Verdict for one rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Nothing rose, nothing fell.
    Ok,
    /// A count rose or a new key appeared.
    Fail,
    /// Counts fell but the baseline was not lowered.
    Stale,
}

impl Status {
    /// Label for the summary table.
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Fail => "FAIL",
            Status::Stale => "STALE",
        }
    }
}

impl Diff {
    /// FAIL beats STALE beats ok.
    pub fn status(&self) -> Status {
        if !self.raised.is_empty() {
            Status::Fail
        } else if !self.lowered.is_empty() {
            Status::Stale
        } else {
            Status::Ok
        }
    }
}

/// Compare current entries with the baseline entries of the same rule.
pub fn compare(now: &BTreeMap<String, u64>, base: &BTreeMap<String, u64>) -> Diff {
    let mut diff = Diff::default();
    for (key, n) in now {
        let b = base.get(key).copied().unwrap_or(0);
        if *n > b {
            diff.raised.push(Change {
                key: key.clone(),
                now: *n,
                base: b,
            });
        } else if *n < b {
            diff.lowered.push(Change {
                key: key.clone(),
                now: *n,
                base: b,
            });
        }
    }
    for (key, b) in base {
        if !now.contains_key(key) {
            diff.lowered.push(Change {
                key: key.clone(),
                now: 0,
                base: *b,
            });
        }
    }
    diff.raised.sort_by(|a, b| a.key.cmp(&b.key));
    diff.lowered.sort_by(|a, b| a.key.cmp(&b.key));
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(&str, u64)]) -> BTreeMap<String, u64> {
        items.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn equal_is_ok() {
        let d = compare(&map(&[("a", 2)]), &map(&[("a", 2)]));
        assert_eq!(d.status(), Status::Ok);
    }

    #[test]
    fn rise_and_new_key_fail() {
        let d = compare(&map(&[("a", 3), ("b", 1)]), &map(&[("a", 2)]));
        assert_eq!(d.status(), Status::Fail);
        assert_eq!(d.raised.len(), 2);
        assert_eq!(
            d.raised[1],
            Change {
                key: "b".into(),
                now: 1,
                base: 0
            }
        );
    }

    #[test]
    fn fall_and_disappearance_are_stale() {
        let d = compare(&map(&[("a", 1)]), &map(&[("a", 2), ("gone", 4)]));
        assert_eq!(d.status(), Status::Stale);
        assert_eq!(d.lowered.len(), 2);
    }

    #[test]
    fn fail_beats_stale() {
        let d = compare(&map(&[("a", 3), ("b", 1)]), &map(&[("a", 2), ("b", 2)]));
        assert_eq!(d.status(), Status::Fail);
        assert_eq!(d.lowered.len(), 1);
    }

    #[test]
    fn outputs_sum_weights_per_key_and_sort() {
        let out = RuleOutput::new(
            "r",
            vec![
                Violation::new("r", "b.rs", 3, "x"),
                Violation::new("r", "a.rs", 9, "x"),
                Violation::new("r", "a.rs", 2, "x"),
                Violation::new("r", "z.rs", 1, "big").weighted(2000),
            ],
            Unit::Count,
        );
        assert_eq!(out.violations[0].file, "a.rs");
        assert_eq!(out.violations[0].line, 2);
        assert_eq!(out.entries["a.rs"], 2);
        assert_eq!(out.total(), 2003);
        let files = RuleOutput::new(
            "size",
            vec![Violation::new("size", "z.rs", 1, "big").weighted(2000)],
            Unit::Files,
        );
        assert_eq!(files.total(), 1);
        assert_eq!(files.entries["z.rs"], 2000);
    }

    #[test]
    fn render_format() {
        assert_eq!(
            Violation::new("threads", "crates/a/src/x.rs", 7, "no").render(),
            "threads  crates/a/src/x.rs:7  no"
        );
    }
}
