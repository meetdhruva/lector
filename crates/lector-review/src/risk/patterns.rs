//! The compiled regex tables behind the deleted-logic and dangerous-construct signals.

use regex::Regex;

use super::{Family, Param, RiskKind, RiskWeights};

pub(super) struct Patterns {
    pub(super) cond: Regex,
    pub(super) err_rust: Regex,
    pub(super) err_script: Regex,
    pub(super) err_python: Regex,
    pub(super) err_go: Regex,
    pub(super) err_jvm: Regex,
    pub(super) err_generic: Regex,
    pub(super) panic_rust: Regex,
    pub(super) panic_go: Regex,
    pub(super) assertion: Regex,
    pub(super) test_decl: Regex,
    pub(super) sql_stmt: Regex,
    pub(super) sql_dynamic: Regex,
    pub(super) danger: Vec<Danger>,
}

pub(super) struct Danger {
    pub(super) re: Regex,
    pub(super) families: &'static [Family],
    pub(super) kind: RiskKind,
    pub(super) param: Param,
    pub(super) what: &'static str,
}

/// The regex tables, compiled once; `None` (signals skipped, no panic) if one fails to compile.
pub(super) fn patterns() -> Option<&'static Patterns> {
    static RISK_PATTERNS: std::sync::OnceLock<Option<Patterns>> = std::sync::OnceLock::new();
    let tables = RISK_PATTERNS.get_or_init(|| compile_patterns().ok());
    tables.as_ref()
}

#[rustfmt::skip]
fn compile_patterns() -> Result<Patterns, regex::Error> {
    let re = Regex::new;
    let d = |s: &str, families: &'static [Family], kind, param, what| -> Result<Danger, regex::Error> {
        Ok(Danger { re: re(s)?, families, kind, param, what })
    };
    use Family::*;
    Ok(Patterns {
        cond: re(r"\b(if|else|elif|match|switch|case|while|guard|unless|when)\b")?,
        err_rust: re(r"(?:\w|\)|\]|>)\?\s*(?:[;.,)}\]]|$)|\.map_err\(|\.ok_or(?:_else)?\(|\.context\(|\.with_context\(|\bErr\(")?,
        err_script: re(r"\bcatch\b|\.catch\(|\bfinally\b|\bthrow\b|\btry\s*\{")?,
        err_python: re(r"\bexcept\b|\bfinally\b|\braise\b|\btry\s*:")?,
        err_go: re(r"\berr\s*!=\s*nil|errors\.(?:Is|As|New)\b|fmt\.Errorf|\brecover\(\)")?,
        err_jvm: re(r"\bcatch\b|\bfinally\b|\bthrows?\b|\btry\b|\bguard\b")?,
        err_generic: re(r"\bcatch\b|\bfinally\b|\brescue\b|\bensure\b|\braise\b|\bthrow\b|\berrno\b|\bperror\b|set\s+-e|\|\|\s*exit|\btrap\b")?,
        panic_rust: re(r"\.unwrap\(\)|\.expect\(|\bpanic!\(|\bunreachable!\(|\btodo!\(|\bunimplemented!\(")?,
        panic_go: re(r"\bpanic\(")?,
        assertion: re(r"\bassert\w*\s*[!(]|\bassert\s|\bexpect\(|\.should\b|\brequire\(")?,
        test_decl: re(r"#\[test\]|#\[tokio::test\]|\bdef test_|\bfunc Test|\b(?:it|test)\(|@Test\b|\bfn test_")?,
        sql_stmt: re(r"(?i)\b(?:select\s.+\sfrom|insert\s+into|update\s+\w+\s+set|delete\s+from)\b")?,
        sql_dynamic: re(r#"\+\s*\w|"\s*\+|\bformat!?\(|\bf"|\bf'|["']\s*%\s*[(\w]|%s[^"']*["']\s*(?:$|;)|\$\{|\.format\(|\{\w*\}|\bSprintf\("#)?,
        danger: vec![
            d(r"\bunsafe\b", &[Rust, Jvm], RiskKind::UnsafeCode, param!(unsafe_code), "`unsafe` code")?,
            d(r#"\bunsafe\.|"unsafe""#, &[Go], RiskKind::UnsafeCode, param!(unsafe_code), "`unsafe` package use")?,
            d(r"\beval\s*\(|\bnew Function\(|\b__import__\(|\bexec\s*\(\s*[^)]", &[Script, Python, Dynamic], RiskKind::DynamicExecution, param!(dynamic_eval), "dynamic code evaluation (`eval`/`exec`)")?,
            d(r"child_process|\bexecSync\(|\bspawnSync\(|\bspawn\(|\bexecFile\(", &[Script], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution")?,
            d(r"\bsubprocess\.|\bos\.system\(|\bos\.popen\(|shell\s*=\s*True", &[Python], RiskKind::DynamicExecution, param!(dynamic_exec), "subprocess/shell execution")?,
            d(r"\bpickle\.loads?\(|\byaml\.load\(|\bmarshal\.loads?\(", &[Python], RiskKind::DynamicExecution, param!(dynamic_exec), "unsafe deserialization")?,
            d(r"Command::new|std::process::Command|\bprocess::Command", &[Rust], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution")?,
            d(r#"\bexec\.Command\(|"os/exec"|\bsyscall\.Exec"#, &[Go], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution")?,
            d(r"Runtime\.getRuntime\(\)\.exec|\bProcessBuilder\b|\bProcess\.Start\(|Class\.forName\(|ObjectInputStream", &[Jvm], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution / reflection / deserialization")?,
            d(r"\b(?:shell_exec|passthru|popen|proc_open|system|exec)\s*\(|%x\{|\bunserialize\(", &[Dynamic], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution")?,
            d(r"\bsystem\s*\(|\bpopen\s*\(|\bstrcpy\s*\(|\bsprintf\s*\(|\bgets\s*\(", &[CLike], RiskKind::DynamicExecution, param!(dynamic_exec), "shell execution / unbounded string function")?,
            d(r"\beval\b|curl[^|]*\|\s*(?:ba)?sh|\bsudo\b|\brm\s+-rf\b|\bchmod\s+(?:-R\s+)?777", &[Shell], RiskKind::DynamicExecution, param!(dynamic_exec), "shell eval/sudo/destructive command")?,
            d(r"\binnerHTML\b|dangerouslySetInnerHTML|document\.write\(", &[Script], RiskKind::DynamicExecution, param!(html_injection), "raw HTML injection")?,
        ],
    })
}

fn count(re: &Regex, lines: &[&str]) -> usize {
    lines.iter().map(|l| re.find_iter(l).count()).sum()
}

pub(super) fn net_added(re: &Regex, removed: &[&str], added: &[&str]) -> isize {
    count(re, added) as isize - count(re, removed) as isize
}

pub(super) fn is_comment(line: &str, prefixes: &[&str]) -> bool {
    let t = line.trim_start();
    prefixes.iter().any(|p| t.starts_with(p))
}
