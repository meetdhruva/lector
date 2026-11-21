//! `arch-check`: the architecture ratchet described in `docs/adr/0008-guardrails.md`.
//!
//! Rules look at crate edges (`cargo metadata`) and call sites (a small lexer, comments and
//! strings stripped, test code skipped). Today's violations are recorded in
//! `arch/baseline.toml`; a check fails when a count rises or a new file appears, and also when
//! a count fell without the baseline being lowered. See `arch/README.md`.
//!
//! [`run`] is the whole command line; `src/main.rs` only forwards `std::env::args`.
#![forbid(unsafe_code)]

pub mod baseline;
pub mod config;
pub mod error;
pub mod lexer;
pub mod meta;
pub mod report;
pub mod rules;
pub mod source;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use baseline::Baseline;
use config::Config;
pub use error::ArchError;
use error::Result;
use report::{compare, total, Change, RuleOutput, Status, Unit};
use source::SourceFile;

/// The ratchet rules, in the order they run and appear in the table.
pub const RULES: &[&str] = &[
    "deps",
    "writes",
    "threads",
    "background",
    "state",
    "dupes",
    "size",
    "purity",
    "hygiene",
];

/// Where the generated crate graph lives.
pub const ARCHITECTURE_DOC: &str = "docs/ARCHITECTURE.md";

const USAGE: &str = "\
usage: arch-check [COMMAND] [OPTIONS]

commands:
  all (default)   run every rule and the graph check; compare with arch/baseline.toml
  deps | writes | threads | background | state | dupes | size | purity | hygiene
                  run one rule
  writes --list-api  print the derived lector-git write API and exit
  graph           print the internal crate graph
                    --write   put it between the markers in docs/ARCHITECTURE.md
                    --check   fail when the committed block differs

options:
  -v, --verbose              list every violation, not only the new ones
  --update                   rewrite arch/baseline.toml; refuses to raise any number
  --allow-increase <reason>  with --update: allow raising/adding entries (initial creation only)
  --moved <old>=<new>        with --update: a file was renamed; move its baseline entries to the
                             new path (refused when the new count is higher). Repeatable.
  --root <dir>               repository root (default: the enclosing cargo workspace)
  -h, --help                 this text

exit codes: 0 ok, 1 violations or stale baseline, 2 usage or internal error";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cmd {
    All,
    Rule(&'static str),
    Graph,
    Help,
}

#[derive(Debug, Clone)]
struct Cli {
    cmd: Cmd,
    verbose: bool,
    update: bool,
    allow_increase: Option<String>,
    moved: Vec<(String, String)>,
    root: Option<PathBuf>,
    graph_write: bool,
    graph_check: bool,
    list_api: bool,
}

/// `<old>=<new>` of `--moved`: two non-empty, different, forward-slash paths.
fn parse_moved(pair: &str) -> Result<(String, String)> {
    let bad = || ArchError::Usage(format!("--moved needs <old>=<new>, got `{pair}`"));
    let (old, new) = pair.split_once('=').ok_or_else(bad)?;
    let (old, new) = (old.trim().replace('\\', "/"), new.trim().replace('\\', "/"));
    if old.is_empty() || new.is_empty() || old == new {
        return Err(bad());
    }
    Ok((old, new))
}

fn parse_args(args: &[String]) -> Result<Cli> {
    let mut cli = Cli {
        cmd: Cmd::All,
        verbose: false,
        update: false,
        allow_increase: None,
        moved: Vec::new(),
        root: None,
        graph_write: false,
        graph_check: false,
        list_api: false,
    };
    let mut positional = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let usage = |m: String| ArchError::Usage(m);
        match a.as_str() {
            "-v" | "--verbose" => cli.verbose = true,
            "--update" => cli.update = true,
            "--write" => cli.graph_write = true,
            "--check" => cli.graph_check = true,
            "--list-api" => cli.list_api = true,
            "-h" | "--help" => cli.cmd = Cmd::Help,
            "--allow-increase" => {
                let reason = it
                    .next()
                    .ok_or_else(|| usage("--allow-increase needs a reason".into()))?;
                cli.allow_increase = Some(reason.clone());
            }
            "--moved" => {
                let pair = it
                    .next()
                    .ok_or_else(|| usage("--moved needs <old>=<new>".into()))?;
                cli.moved.push(parse_moved(pair)?);
            }
            "--root" => {
                let dir = it
                    .next()
                    .ok_or_else(|| usage("--root needs a directory".into()))?;
                cli.root = Some(PathBuf::from(dir));
            }
            s if s.starts_with("--allow-increase=") => {
                cli.allow_increase = Some(s["--allow-increase=".len()..].to_string());
            }
            s if s.starts_with("--moved=") => {
                cli.moved.push(parse_moved(&s["--moved=".len()..])?);
            }
            s if s.starts_with("--root=") => cli.root = Some(PathBuf::from(&s["--root=".len()..])),
            s if s.starts_with('-') => return Err(usage(format!("unknown option `{s}`"))),
            s => {
                if positional.replace(s.to_string()).is_some() {
                    return Err(usage(format!("unexpected argument `{s}`")));
                }
            }
        }
    }
    if cli.cmd != Cmd::Help {
        cli.cmd = match positional.as_deref() {
            None | Some("all") => Cmd::All,
            Some("graph") => Cmd::Graph,
            Some(name) => match RULES.iter().find(|r| **r == name) {
                Some(r) => Cmd::Rule(r),
                None => return Err(ArchError::Usage(format!("unknown command `{name}`"))),
            },
        };
    }
    let bad = |m: &str| Err(ArchError::Usage(m.to_string()));
    if cli.graph_write && cli.graph_check {
        return bad("--write and --check are exclusive");
    }
    if (cli.graph_write || cli.graph_check) && cli.cmd != Cmd::Graph && cli.cmd != Cmd::Help {
        return bad("--write and --check belong to `graph`");
    }
    if cli.list_api && cli.cmd != Cmd::Rule("writes") {
        return bad("--list-api belongs to `writes`");
    }
    if cli.cmd == Cmd::Graph && cli.update {
        return bad("--update does not apply to `graph`");
    }
    if cli.allow_increase.is_some() && !cli.update {
        return bad("--allow-increase only makes sense with --update");
    }
    if !cli.moved.is_empty() && !cli.update {
        return bad("--moved only makes sense with --update");
    }
    if cli
        .allow_increase
        .as_deref()
        .is_some_and(|r| r.trim().is_empty())
    {
        return bad("--allow-increase needs a non-empty reason");
    }
    Ok(cli)
}

/// Run the command line `args` (without the program name). Returns the process exit code:
/// 0 ok, 1 violations or stale baseline, 2 usage or internal error.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = parse_args(args).and_then(|cli| {
        if cli.cmd == Cmd::Help {
            emit(out, USAGE)?;
            return Ok(0);
        }
        execute(&cli, out)
    });
    match result {
        Ok(code) => code,
        Err(ArchError::Usage(m)) => {
            let _ = writeln!(err, "arch-check: {m}\n\n{USAGE}");
            2
        }
        Err(e) => {
            let _ = writeln!(err, "arch-check: error: {e}");
            2
        }
    }
}

fn emit(out: &mut dyn Write, line: &str) -> Result<()> {
    writeln!(out, "{line}").map_err(|e| ArchError::io("<output>", e))
}

fn find_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let text = fs::read_to_string(dir.join("Cargo.toml")).ok()?;
        text.contains("[workspace]").then(|| dir.to_path_buf())
    })
}

fn execute(cli: &Cli, out: &mut dyn Write) -> Result<i32> {
    let root = match &cli.root {
        Some(r) => r.clone(),
        None => {
            let cwd = std::env::current_dir().map_err(|e| ArchError::io(".", e))?;
            find_root(&cwd).ok_or_else(|| {
                ArchError::Usage("not inside a cargo workspace; pass --root <dir>".into())
            })?
        }
    };
    let cfg = Config::load(&root)?;
    if cli.list_api {
        let files = source::load_sources(&root)?;
        let api = rules::writes::derive_write_api(&files, &cfg.allow("git-read"));
        for name in api.names.keys() {
            emit(out, name)?;
        }
        return Ok(0);
    }
    match cli.cmd {
        Cmd::Graph => graph_command(cli, &root, &cfg, out),
        Cmd::All => ratchet(cli, &root, &cfg, RULES, true, out),
        Cmd::Rule(r) => ratchet(cli, &root, &cfg, &[r], false, out),
        Cmd::Help => Ok(0),
    }
}

fn require_layers(cfg: &Config) -> Result<&config::Layers> {
    cfg.layers
        .as_ref()
        .ok_or_else(|| ArchError::Config("arch/layers.toml not found".into()))
}

fn load_workspace(root: &Path) -> Result<meta::Workspace> {
    meta::parse_workspace(&meta::run_cargo_metadata(root, true)?, root)
}

fn graph_diagram(root: &Path, cfg: &Config) -> Result<String> {
    let layers = require_layers(cfg)?;
    let ws = load_workspace(root)?;
    Ok(rules::graph::render(&ws, layers))
}

fn read_doc(root: &Path) -> Result<String> {
    fs::read_to_string(root.join(ARCHITECTURE_DOC)).map_err(|e| ArchError::io(ARCHITECTURE_DOC, e))
}

fn graph_command(cli: &Cli, root: &Path, cfg: &Config, out: &mut dyn Write) -> Result<i32> {
    let diagram = graph_diagram(root, cfg)?;
    if cli.graph_write {
        let doc = read_doc(root)?;
        let updated = rules::graph::write_into(&doc, &diagram).map_err(ArchError::Config)?;
        if updated != doc {
            fs::write(root.join(ARCHITECTURE_DOC), updated)
                .map_err(|e| ArchError::io(ARCHITECTURE_DOC, e))?;
            emit(
                out,
                &format!("graph: wrote the crate graph into {ARCHITECTURE_DOC}"),
            )?;
        } else {
            emit(
                out,
                &format!("graph: {ARCHITECTURE_DOC} is already current"),
            )?;
        }
        return Ok(0);
    }
    if cli.graph_check {
        return match graph_state(root, &diagram)? {
            rules::graph::State::Current => {
                emit(out, "graph: ok")?;
                Ok(0)
            }
            _ => {
                emit(out, &graph_failure_message())?;
                Ok(1)
            }
        };
    }
    out.write_all(diagram.as_bytes())
        .map_err(|e| ArchError::io("<output>", e))?;
    Ok(0)
}

fn graph_failure_message() -> String {
    format!(
        "graph  {ARCHITECTURE_DOC}:1  the generated crate graph is missing or stale: run scripts/arch-check graph --write"
    )
}

fn graph_state(root: &Path, diagram: &str) -> Result<rules::graph::State> {
    Ok(rules::graph::check(&read_doc(root)?, diagram))
}

/// Everything the rules need, loaded lazily so `arch-check size` does not run cargo.
struct Inputs<'a> {
    root: &'a Path,
    cfg: &'a Config,
    files: Option<Vec<SourceFile>>,
    ws: Option<meta::Workspace>,
}

impl Inputs<'_> {
    fn files(&mut self) -> Result<&[SourceFile]> {
        if self.files.is_none() {
            self.files = Some(source::load_sources(self.root)?);
        }
        Ok(self.files.as_deref().unwrap_or(&[]))
    }

    fn workspace(&mut self) -> Result<&meta::Workspace> {
        if self.ws.is_none() {
            self.ws = Some(load_workspace(self.root)?);
        }
        Ok(self.ws.get_or_insert_with(Default::default))
    }
}

fn run_rule(name: &str, inputs: &mut Inputs<'_>) -> Result<RuleOutput> {
    let cfg = inputs.cfg;
    Ok(match name {
        "deps" => {
            let layers = require_layers(cfg)?.clone();
            let ws = inputs.workspace()?.clone();
            let resolve = meta::parse_resolve(&meta::run_cargo_metadata(inputs.root, false)?)?;
            rules::deps::check(&ws, Some(&resolve), &layers)
        }
        "writes" => rules::writes::check(inputs.files()?, cfg),
        "threads" => rules::threads::check(inputs.files()?, cfg),
        "background" => rules::background::check(inputs.files()?, cfg),
        "state" => rules::state::check(inputs.files()?, cfg),
        "dupes" => rules::dupes::check(inputs.files()?, cfg),
        "size" => rules::size::check(inputs.files()?, cfg),
        "purity" => rules::purity::check(inputs.files()?, cfg),
        "hygiene" => {
            let ws = inputs.workspace()?.clone();
            rules::hygiene::check(&ws, inputs.files()?, cfg)
        }
        other => return Err(ArchError::Usage(format!("unknown rule `{other}`"))),
    })
}

fn unit_label(unit: Unit) -> &'static str {
    match unit {
        Unit::Count => "violations",
        Unit::Files => "files over the limit",
    }
}

struct Row {
    rule: String,
    now: String,
    base: String,
    status: String,
    unit: &'static str,
}

fn ratchet(
    cli: &Cli,
    root: &Path,
    cfg: &Config,
    rule_names: &[&str],
    with_graph: bool,
    out: &mut dyn Write,
) -> Result<i32> {
    let mut inputs = Inputs {
        root,
        cfg,
        files: None,
        ws: None,
    };
    let mut outputs: Vec<RuleOutput> = Vec::new();
    for name in rule_names {
        outputs.push(run_rule(name, &mut inputs)?);
    }
    let loaded = Baseline::load(root)?;
    let mut baseline = loaded.clone().unwrap_or_default();

    if cli.update {
        match update_baseline(cli, &outputs, &mut baseline, out)? {
            UpdateResult::Refused => return Ok(1),
            UpdateResult::Written | UpdateResult::Unchanged => {}
        }
        if loaded.as_ref() != Some(&baseline) {
            baseline.save(root)?;
        }
    } else if loaded.is_none() {
        emit(
            out,
            "note: arch/baseline.toml not found; every violation counts as new (create it once with --update --allow-increase \"<reason>\")",
        )?;
    }

    let mut rows = Vec::new();
    let mut failed = false;
    let mut details: Vec<String> = Vec::new();
    for o in &outputs {
        let base = baseline.entries(o.rule);
        let diff = compare(&o.entries, &base);
        let status = diff.status();
        failed |= status != Status::Ok;
        rows.push(Row {
            rule: o.rule.to_string(),
            now: o.total().to_string(),
            base: total(&base, o.unit).to_string(),
            status: status.label().to_string(),
            unit: unit_label(o.unit),
        });
        if let Some(n) = &o.notice {
            details.push(format!("note: {}: {n}", o.rule));
        }
        if status == Status::Fail {
            details.push(format!("{}: new or increased (baseline -> now):", o.rule));
            let mut shown = 0;
            for c in &diff.raised {
                details.push(format!("  {}: {} -> {}", c.key, c.base, c.now));
                for v in o.violations.iter().filter(|v| v.key == c.key) {
                    if shown < 200 {
                        details.push(format!("    {}", v.render()));
                    }
                    shown += 1;
                }
            }
            if shown > 200 {
                details.push(format!("    ... {} more (use --verbose)", shown - 200));
            }
        }
        if !diff.lowered.is_empty() {
            details.push(format!(
                "{}: lowered, but the baseline still says more:",
                o.rule
            ));
            for c in &diff.lowered {
                details.push(format!("  {}: baseline {} -> now {}", c.key, c.base, c.now));
            }
        }
    }
    if with_graph {
        let state = graph_state(root, &graph_diagram(root, cfg)?)?;
        let ok = state == rules::graph::State::Current;
        failed |= !ok;
        rows.push(Row {
            rule: "graph".into(),
            now: if ok { "0".into() } else { "1".into() },
            base: "-".into(),
            status: if ok { "ok".into() } else { "FAIL".into() },
            unit: "crate graph in docs/ARCHITECTURE.md",
        });
        if !ok {
            details.push(graph_failure_message());
        }
    }

    print_table(out, &rows)?;
    for d in &details {
        emit(out, d)?;
    }
    if cli.verbose {
        emit(out, "")?;
        emit(out, "all violations:")?;
        for o in &outputs {
            for v in &o.violations {
                emit(out, &v.render())?;
            }
        }
    }
    if outputs
        .iter()
        .any(|o| compare(&o.entries, &baseline.entries(o.rule)).status() == Status::Stale)
    {
        emit(out, "baseline is stale: run arch-check --update")?;
    }
    Ok(i32::from(failed))
}

fn print_table(out: &mut dyn Write, rows: &[Row]) -> Result<()> {
    emit(
        out,
        &format!(
            "{:<11} {:>6} {:>9}  {:<6} {}",
            "rule", "now", "baseline", "status", ""
        ),
    )?;
    for r in rows {
        emit(
            out,
            &format!(
                "{:<11} {:>6} {:>9}  {:<6} {}",
                r.rule, r.now, r.base, r.status, r.unit
            ),
        )?;
    }
    Ok(())
}

/// `--moved old=new`: in every rule that ran, the baseline entry of `old` becomes the entry of
/// `new` with the same count. The usual comparison then decides: a `new` that violates more
/// than `old` did is a raise and is refused like any other. An `old` without an entry in any
/// rule that ran, or a `new` that already has one, is a usage error (a typo must not pass).
fn transfer_moves(
    baseline: &mut Baseline,
    outputs: &[RuleOutput],
    moved: &[(String, String)],
    out: &mut dyn Write,
) -> Result<()> {
    for (old, new) in moved {
        let mut found = false;
        for o in outputs {
            let Some(entries) = baseline.rules.get_mut(o.rule) else {
                continue;
            };
            let Some(count) = entries.remove(old) else {
                continue;
            };
            if entries.contains_key(new) {
                return Err(ArchError::Usage(format!(
                    "--moved {old}={new}: [{}] already has an entry for `{new}`",
                    o.rule
                )));
            }
            entries.insert(new.clone(), count);
            found = true;
            emit(out, &format!("moved  {}  {old} -> {new} ({count})", o.rule))?;
        }
        if !found {
            return Err(ArchError::Usage(format!(
                "--moved {old}={new}: no baseline entry for `{old}` in the rules that ran"
            )));
        }
    }
    Ok(())
}

enum UpdateResult {
    Refused,
    Written,
    Unchanged,
}

/// Replace the sections of the rules that ran. Raising a number or adding an entry needs
/// `--allow-increase`; lowering is always fine.
fn update_baseline(
    cli: &Cli,
    outputs: &[RuleOutput],
    baseline: &mut Baseline,
    out: &mut dyn Write,
) -> Result<UpdateResult> {
    transfer_moves(baseline, outputs, &cli.moved, out)?;
    let mut raised: Vec<(String, Change)> = Vec::new();
    let mut lowered = 0usize;
    for o in outputs {
        let diff = compare(&o.entries, &baseline.entries(o.rule));
        lowered += diff.lowered.len();
        raised.extend(diff.raised.into_iter().map(|c| (o.rule.to_string(), c)));
    }
    if !raised.is_empty() {
        match &cli.allow_increase {
            None => {
                emit(
                    out,
                    "refusing to raise the baseline; these entries are new or higher than recorded:",
                )?;
                for (rule, c) in &raised {
                    emit(
                        out,
                        &format!("  {rule}  {}: {} -> {}", c.key, c.base, c.now),
                    )?;
                }
                emit(
                    out,
                    "fix the code instead; `--allow-increase \"<reason>\"` exists for the initial creation of the baseline only",
                )?;
                return Ok(UpdateResult::Refused);
            }
            Some(reason) => {
                emit(
                    out,
                    &format!(
                        "ALLOWED INCREASE (reason: {reason}): {} baseline entries raised or added",
                        raised.len()
                    ),
                )?;
                for (rule, c) in &raised {
                    emit(
                        out,
                        &format!("  {rule}  {}: {} -> {}", c.key, c.base, c.now),
                    )?;
                }
            }
        }
    }
    let before = baseline.clone();
    for o in outputs {
        baseline.rules.insert(o.rule.to_string(), o.entries.clone());
    }
    if *baseline == before {
        emit(out, "baseline already up to date")?;
        return Ok(UpdateResult::Unchanged);
    }
    emit(
        out,
        &format!(
            "baseline updated: {lowered} entries lowered or removed, {} raised or added",
            raised.len()
        ),
    )?;
    Ok(UpdateResult::Written)
}

/// Per-rule baseline counts, for tooling and tests.
pub fn baseline_totals(baseline: &Baseline) -> BTreeMap<String, u64> {
    baseline
        .rules
        .iter()
        .map(|(rule, entries)| {
            let unit = if rule == "size" {
                Unit::Files
            } else {
                Unit::Count
            };
            (rule.clone(), total(entries, unit))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn argument_parsing() {
        let c = parse_args(&args("size --verbose")).unwrap();
        assert_eq!(c.cmd, Cmd::Rule("size"));
        assert!(c.verbose);
        assert_eq!(parse_args(&[]).unwrap().cmd, Cmd::All);
        let c = parse_args(&[
            "--update".into(),
            "--allow-increase".into(),
            "initial".into(),
        ])
        .unwrap();
        assert!(c.update);
        assert_eq!(c.allow_increase.as_deref(), Some("initial"));
        assert_eq!(parse_args(&args("graph --check")).unwrap().cmd, Cmd::Graph);
    }

    #[test]
    fn moved_arguments() {
        let c = parse_args(&args("--update --moved a/x.rs=b/x.rs --moved=c.rs=d.rs")).unwrap();
        assert_eq!(
            c.moved,
            [
                ("a/x.rs".to_string(), "b/x.rs".to_string()),
                ("c.rs".to_string(), "d.rs".to_string())
            ]
        );
        for bad in [
            "--moved a=b",
            "--update --moved",
            "--update --moved a",
            "--update --moved a=a",
            "--update --moved =b",
        ] {
            assert!(
                matches!(parse_args(&args(bad)), Err(ArchError::Usage(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn argument_errors() {
        for bad in [
            "nope",
            "size extra",
            "--bogus",
            "size --write",
            "graph --write --check",
            "graph --update",
            "--allow-increase",
            "all --allow-increase=x",
        ] {
            assert!(
                matches!(parse_args(&args(bad)), Err(ArchError::Usage(_))),
                "{bad}"
            );
        }
        assert!(parse_args(&["--update".into(), "--allow-increase".into(), "  ".into()]).is_err());
    }
}
