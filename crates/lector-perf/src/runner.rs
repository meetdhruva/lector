//! The `perf-gate` runner: runs every gate binary under `valgrind --tool=callgrind`, reads the
//! instruction counts, and compares them with the baseline (`perf/baseline.json`).
//!
//! The pure parts (output parsing, baseline file, comparison, table) are unit-tested; the
//! process-spawning parts are exercised by `scripts/perf-gate.sh` in CI.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{anyhow, bail, Context as _};
use serde_json::{json, Value};

/// The gate binaries, in the order they run.
pub const GATE_BINS: &[&str] = &[
    "gate_text",
    "gate_buffer",
    "gate_diff",
    "gate_search",
    "gate_syntax",
];

/// Format version of the baseline file.
pub const BASELINE_SCHEMA: u64 = 1;
/// Default allowed regression, in percent.
pub const DEFAULT_THRESHOLD_PCT: f64 = 5.0;
/// Runs of a threaded benchmark; the minimum is kept.
pub const DEFAULT_REPEATS: u32 = 3;

/// A benchmark as listed by a gate binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// `group/name`.
    pub name: String,
    /// Counted by process difference (work on other threads) instead of the toggle window.
    pub threaded: bool,
}

/// Parses the output of `gate_x --list` (`name<TAB>window|process`).
pub fn parse_list(output: &str) -> Vec<Listed> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (name, mode) = l.split_once('\t').unwrap_or((l, "window"));
            Listed {
                name: name.trim().to_string(),
                threaded: mode.trim() == "process",
            }
        })
        .collect()
}

/// The `summary:` line of a callgrind output file: the instructions collected.
pub fn parse_callgrind_summary(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("summary:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// What a baseline is only valid for: instruction counts change with the compiler, its
/// optimization settings and (a little) the C library.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Toolchain {
    /// `rustc` release, e.g. `1.97.0`.
    pub rustc: String,
    /// Host triple.
    pub host: String,
    /// LLVM version `rustc` is built with.
    pub llvm: String,
    /// Identifier of the build settings (`PERF_GATE_PROFILE`, set by `scripts/perf-gate.sh`).
    pub profile: String,
    /// Valgrind version (informational).
    pub valgrind: String,
    /// C library version (informational).
    pub libc: String,
}

impl Toolchain {
    /// Parses the output of `rustc -vV`.
    pub fn from_rustc_vv(text: &str, profile: &str, valgrind: &str, libc: &str) -> Toolchain {
        let field = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        Toolchain {
            rustc: field("release:"),
            host: field("host:"),
            llvm: field("LLVM version:"),
            profile: profile.to_string(),
            valgrind: valgrind.to_string(),
            libc: libc.to_string(),
        }
    }

    /// Why counts measured with `self` cannot be compared with a baseline of `baseline`
    /// (`None` when they can).
    pub fn mismatch(&self, baseline: &Toolchain) -> Option<String> {
        let mut diffs = Vec::new();
        let mut check = |what: &str, mine: &str, theirs: &str| {
            if mine != theirs {
                diffs.push(format!("{what} {theirs:?} (baseline) vs {mine:?} (here)"));
            }
        };
        check("rustc", &self.rustc, &baseline.rustc);
        check("host", &self.host, &baseline.host);
        check("LLVM", &self.llvm, &baseline.llvm);
        check("profile", &self.profile, &baseline.profile);
        (!diffs.is_empty()).then(|| diffs.join(", "))
    }

    /// Differences that do not block the comparison but may explain small drifts.
    pub fn soft_differences(&self, baseline: &Toolchain) -> Option<String> {
        let mut diffs = Vec::new();
        if !baseline.valgrind.is_empty() && self.valgrind != baseline.valgrind {
            diffs.push(format!(
                "valgrind {} vs {}",
                baseline.valgrind, self.valgrind
            ));
        }
        if !baseline.libc.is_empty() && self.libc != baseline.libc {
            diffs.push(format!("libc {} vs {}", baseline.libc, self.libc));
        }
        (!diffs.is_empty()).then(|| diffs.join(", "))
    }
}

/// `perf/baseline.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Baseline {
    /// The environment the counts were measured in.
    pub toolchain: Toolchain,
    /// Instructions per benchmark.
    pub benches: BTreeMap<String, u64>,
}

impl Baseline {
    /// The JSON document (keys sorted, so diffs are minimal).
    pub fn to_json(&self) -> Value {
        json!({
            "schema": BASELINE_SCHEMA,
            "note": "Instruction counts (callgrind Ir) of fixed workloads; regenerate with scripts/perf-gate.sh --update in the commit that bumps the toolchain.",
            "rustc": self.toolchain.rustc,
            "host": self.toolchain.host,
            "llvm": self.toolchain.llvm,
            "profile": self.toolchain.profile,
            "valgrind": self.toolchain.valgrind,
            "libc": self.toolchain.libc,
            "benches": self.benches,
        })
    }

    /// Parses a baseline document.
    pub fn from_json(text: &str) -> anyhow::Result<Baseline> {
        let v: Value = serde_json::from_str(text).context("the baseline is not valid JSON")?;
        let str_of = |key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let schema = v.get("schema").and_then(Value::as_u64).unwrap_or(0);
        if schema != BASELINE_SCHEMA {
            bail!("unsupported baseline schema {schema} (expected {BASELINE_SCHEMA})");
        }
        let benches = v
            .get("benches")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("the baseline has no \"benches\" object"))?
            .iter()
            .map(|(k, n)| {
                n.as_u64()
                    .map(|n| (k.clone(), n))
                    .ok_or_else(|| anyhow!("baseline entry {k:?} is not a number"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Baseline {
            toolchain: Toolchain {
                rustc: str_of("rustc"),
                host: str_of("host"),
                llvm: str_of("llvm"),
                profile: str_of("profile"),
                valgrind: str_of("valgrind"),
                libc: str_of("libc"),
            },
            benches,
        })
    }
}

/// How one benchmark compares with its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Within the threshold.
    Ok,
    /// More instructions than allowed.
    Regressed,
    /// Fewer instructions than the threshold: update the baseline to lock the gain in.
    Improved,
    /// Measured but not in the baseline.
    New,
    /// In the baseline but not measured.
    Missing,
}

/// One row of the comparison table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Benchmark name.
    pub name: String,
    /// Baseline count.
    pub baseline: Option<u64>,
    /// Measured count.
    pub measured: Option<u64>,
    /// Change in percent (positive = more instructions).
    pub delta_pct: Option<f64>,
    /// The verdict.
    pub verdict: Verdict,
}

/// Compares `measured` with `baseline`. A benchmark regresses when it needs more than
/// `threshold_pct` percent more instructions. With `partial` (a subset was run), benchmarks
/// that were not run are not reported as missing.
pub fn compare(
    baseline: &BTreeMap<String, u64>,
    measured: &BTreeMap<String, u64>,
    threshold_pct: f64,
    partial: bool,
) -> Vec<Row> {
    let mut names: Vec<&String> = baseline.keys().chain(measured.keys()).collect();
    names.sort();
    names.dedup();
    let mut rows = Vec::new();
    for name in names {
        let (b, m) = (baseline.get(name).copied(), measured.get(name).copied());
        let row = match (b, m) {
            (Some(b), Some(m)) => {
                let delta = if b == 0 {
                    0.0
                } else {
                    (m as f64 - b as f64) / b as f64 * 100.0
                };
                let verdict = if delta > threshold_pct {
                    Verdict::Regressed
                } else if delta < -threshold_pct {
                    Verdict::Improved
                } else {
                    Verdict::Ok
                };
                Row {
                    name: name.clone(),
                    baseline: Some(b),
                    measured: Some(m),
                    delta_pct: Some(delta),
                    verdict,
                }
            }
            (None, Some(m)) => Row {
                name: name.clone(),
                baseline: None,
                measured: Some(m),
                delta_pct: None,
                verdict: Verdict::New,
            },
            (Some(b), None) => {
                if partial {
                    continue;
                }
                Row {
                    name: name.clone(),
                    baseline: Some(b),
                    measured: None,
                    delta_pct: None,
                    verdict: Verdict::Missing,
                }
            }
            (None, None) => continue,
        };
        rows.push(row);
    }
    rows
}

/// `1234567` as `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The comparison as an aligned text table.
pub fn render_table(rows: &[Row], threshold_pct: f64) -> String {
    let width = rows.iter().map(|r| r.name.len()).max().unwrap_or(9).max(9);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<width$}  {:>15}  {:>15}  {:>8}  verdict",
        "benchmark", "baseline", "measured", "change"
    );
    for r in rows {
        let cell = |n: Option<u64>| n.map_or("-".to_string(), thousands);
        let change = r.delta_pct.map_or("-".to_string(), |d| format!("{d:+.2}%"));
        let verdict = match r.verdict {
            Verdict::Ok => "ok".to_string(),
            Verdict::Regressed => format!("REGRESSED (> {threshold_pct}%)"),
            Verdict::Improved => "improved (consider --update)".to_string(),
            Verdict::New => "new (not in baseline)".to_string(),
            Verdict::Missing => "MISSING (not measured)".to_string(),
        };
        let _ = writeln!(
            out,
            "{:<width$}  {:>15}  {:>15}  {:>8}  {verdict}",
            r.name,
            cell(r.baseline),
            cell(r.measured),
            change
        );
    }
    out
}

/// Command-line options of `perf-gate`.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Directory holding the gate binaries (`target/release`).
    pub bin_dir: PathBuf,
    /// The baseline file.
    pub baseline: PathBuf,
    /// Also write the measured numbers here (same format as the baseline).
    pub results: Option<PathBuf>,
    /// Allowed regression in percent.
    pub threshold_pct: f64,
    /// Rewrite the baseline instead of comparing.
    pub update: bool,
    /// Only benchmarks whose name contains this.
    pub only: Option<String>,
    /// Do not fail when a gate binary is missing.
    pub allow_missing: bool,
    /// Runs per threaded benchmark.
    pub repeats: u32,
    /// The valgrind executable.
    pub valgrind: String,
    /// Pin the measured process to one CPU (`taskset`), so that `available_parallelism()` and
    /// therefore the number of worker threads is the same on every machine.
    pub pin_cpu: bool,
}

/// Usage text.
pub const USAGE: &str = "\
perf-gate: instruction-count gates (valgrind --tool=callgrind)

usage: perf-gate [options]
  --bin-dir DIR        gate binaries (default: target/release)
  --baseline FILE      baseline (default: perf/baseline.json)
  --results FILE       also write the measured numbers (baseline format)
  --threshold PCT      allowed regression in percent (default 5; env PERF_GATE_THRESHOLD)
  --update             rewrite the baseline with the measured numbers
  --only SUBSTRING     run only matching benchmarks (with --update: merge into the baseline)
  --allow-missing      do not fail when a gate binary was not built (implied by --only)
  --repeats N          runs per multi-threaded benchmark, minimum kept (default 3)
  --valgrind PATH      valgrind executable (default: valgrind)
  --no-pin             do not pin the measured process to one CPU with taskset

Environment: PERF_GATE_PROFILE names the build settings recorded in the baseline.
Exit code: 0 pass (or toolchain differs: skipped with a warning), 1 regression or error, 2 usage.
";

/// Parses the command line (without the program name). `Ok(None)` means help was requested.
pub fn parse_args(args: &[String], env_threshold: Option<&str>) -> anyhow::Result<Option<Options>> {
    let mut o = Options {
        bin_dir: PathBuf::from("target/release"),
        baseline: PathBuf::from("perf/baseline.json"),
        results: None,
        threshold_pct: DEFAULT_THRESHOLD_PCT,
        update: false,
        only: None,
        allow_missing: false,
        repeats: DEFAULT_REPEATS,
        valgrind: "valgrind".to_string(),
        pin_cpu: true,
    };
    if let Some(t) = env_threshold.filter(|t| !t.is_empty()) {
        o.threshold_pct = parse_pct(t)?;
    }
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--bin-dir" => o.bin_dir = PathBuf::from(value("--bin-dir")?),
            "--baseline" => o.baseline = PathBuf::from(value("--baseline")?),
            "--results" => o.results = Some(PathBuf::from(value("--results")?)),
            "--threshold" => o.threshold_pct = parse_pct(&value("--threshold")?)?,
            "--update" => o.update = true,
            "--only" => o.only = Some(value("--only")?),
            "--allow-missing" => o.allow_missing = true,
            "--repeats" => {
                o.repeats = value("--repeats")?
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| anyhow!("--repeats needs a positive number"))?;
            }
            "--valgrind" => o.valgrind = value("--valgrind")?,
            "--no-pin" => o.pin_cpu = false,
            other => bail!("unknown option {other:?}"),
        }
    }
    Ok(Some(o))
}

fn parse_pct(text: &str) -> anyhow::Result<f64> {
    text.trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|p| p.is_finite() && *p >= 0.0)
        .ok_or_else(|| anyhow!("{text:?} is not a percentage"))
}

fn tool_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The toolchain this run is measuring with.
pub fn detect_toolchain(valgrind: &str) -> anyhow::Result<Toolchain> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let vv = tool_output(&rustc, &["-vV"]).ok_or_else(|| anyhow!("cannot run `{rustc} -vV`"))?;
    let valgrind_version = tool_output(valgrind, &["--version"])
        .ok_or_else(|| {
            anyhow!(
                "cannot run `{valgrind}`: install valgrind (Debian/Ubuntu: sudo apt-get install valgrind)"
            )
        })?
        .trim()
        .to_string();
    let libc = tool_output("ldd", &["--version"])
        .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_default();
    let profile = std::env::var("PERF_GATE_PROFILE").unwrap_or_default();
    Ok(Toolchain::from_rustc_vv(
        &vv,
        &profile,
        &valgrind_version,
        &libc,
    ))
}

/// The first CPU this process may run on, from the `Cpus_allowed_list` line of
/// `/proc/self/status` (`0-3`, `2,5`).
pub fn first_allowed_cpu(status: &str) -> Option<u32> {
    let list = status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?;
    let first = list.trim().split([',', '-']).next()?;
    first.trim().parse().ok()
}

/// The CPU to pin measured processes to, when pinning is on and possible.
fn pin_cpu(o: &Options) -> Option<u32> {
    if !o.pin_cpu {
        return None;
    }
    // `taskset` must exist and the CPU must be allowed for us.
    tool_output("taskset", &["--version"])?;
    first_allowed_cpu(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// Runs `bin args...` under callgrind and returns the collected instruction count.
fn callgrind(
    o: &Options,
    bin: &Path,
    args: &[&str],
    window: bool,
    scratch: &Path,
) -> anyhow::Result<u64> {
    let out_file = scratch.join("callgrind.out");
    let _ = std::fs::remove_file(&out_file);
    let mut cmd = match pin_cpu(o) {
        Some(cpu) => {
            let mut c = Command::new("taskset");
            c.arg("-c").arg(cpu.to_string()).arg(&o.valgrind);
            c
        }
        None => Command::new(&o.valgrind),
    };
    cmd.arg("--tool=callgrind")
        .arg("--quiet")
        .arg(format!("--callgrind-out-file={}", out_file.display()));
    if window {
        cmd.arg("--collect-atstart=no")
            .arg("--toggle-collect=*gate_window*");
    }
    cmd.arg(bin)
        .args(args)
        .env_remove("RUST_LOG")
        .env("RAYON_NUM_THREADS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = cmd
        .output()
        .with_context(|| format!("cannot run {}", o.valgrind))?;
    if !output.status.success() {
        bail!(
            "{} {} failed ({}): {}",
            bin.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = std::fs::read_to_string(&out_file)
        .with_context(|| format!("callgrind wrote no {}", out_file.display()))?;
    parse_callgrind_summary(&text).ok_or_else(|| anyhow!("no summary in the callgrind output"))
}

/// Measures one benchmark.
fn measure(o: &Options, bin: &Path, b: &Listed, scratch: &Path) -> anyhow::Result<u64> {
    if !b.threaded {
        let n = callgrind(o, bin, &[&b.name], true, scratch)?;
        if n == 0 {
            bail!(
                "{}: no instructions in the measured window (did the toggle match?)",
                b.name
            );
        }
        return Ok(n);
    }
    // Work on other threads is not seen by the toggle: count everything and subtract the setup.
    let setup = callgrind(o, bin, &[&b.name, "--setup-only"], false, scratch)?;
    let mut best: Option<u64> = None;
    for _ in 0..o.repeats {
        let total = callgrind(o, bin, &[&b.name], false, scratch)?;
        let work = total.saturating_sub(setup);
        best = Some(best.map_or(work, |m| m.min(work)));
    }
    best.filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("{}: the work added no instructions", b.name))
}

/// Runs the gate. Returns the process exit code.
pub fn run(o: &Options) -> anyhow::Result<ExitCode> {
    let here = detect_toolchain(&o.valgrind)?;
    let scratch = tempfile::tempdir().context("cannot create a temporary directory")?;
    let mut measured: BTreeMap<String, u64> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    for gate in GATE_BINS {
        let bin = o
            .bin_dir
            .join(format!("{gate}{}", std::env::consts::EXE_SUFFIX));
        if !bin.is_file() {
            let msg = format!("{gate} was not built ({})", bin.display());
            if o.allow_missing || o.only.is_some() {
                eprintln!("warning: {msg}: its benchmarks are skipped");
            } else {
                failures.push(msg);
            }
            continue;
        }
        let listing = match tool_output(&bin.to_string_lossy(), &["--list"]) {
            Some(l) => parse_list(&l),
            None => {
                failures.push(format!("{gate} --list failed"));
                continue;
            }
        };
        for b in listing
            .iter()
            .filter(|b| o.only.as_deref().is_none_or(|s| b.name.contains(s)))
        {
            match measure(o, &bin, b, scratch.path()) {
                Ok(n) => {
                    eprintln!("  {:<36} {:>16}", b.name, thousands(n));
                    measured.insert(b.name.clone(), n);
                }
                Err(e) => failures.push(format!("{}: {e:#}", b.name)),
            }
        }
    }
    for f in &failures {
        eprintln!("error: {f}");
    }

    let current = Baseline {
        toolchain: here.clone(),
        benches: measured.clone(),
    };
    if let Some(path) = &o.results {
        write_baseline(path, &current)?;
        eprintln!("measured numbers written to {}", path.display());
    }

    if o.update {
        if !failures.is_empty() {
            bail!("not updating the baseline: some benchmarks failed");
        }
        let mut next = current;
        if o.only.is_some() {
            // Merge into the existing baseline of the same toolchain.
            if let Ok(text) = std::fs::read_to_string(&o.baseline) {
                if let Ok(old) = Baseline::from_json(&text) {
                    if here.mismatch(&old.toolchain).is_none() {
                        let mut merged = old.benches;
                        merged.extend(measured);
                        next.benches = merged;
                    }
                }
            }
        }
        write_baseline(&o.baseline, &next)?;
        println!(
            "baseline updated: {} ({} benchmarks, rustc {}, {})",
            o.baseline.display(),
            next.benches.len(),
            here.rustc,
            here.host
        );
        return Ok(ExitCode::SUCCESS);
    }

    let text = std::fs::read_to_string(&o.baseline).with_context(|| {
        format!(
            "cannot read the baseline {} (create it with scripts/perf-gate.sh --update)",
            o.baseline.display()
        )
    })?;
    let baseline = Baseline::from_json(&text)?;
    if let Some(why) = here.mismatch(&baseline.toolchain) {
        println!(
            "WARNING: the baseline was recorded with a different toolchain ({why}); instruction \
             counts are not comparable, so the gate is SKIPPED (not failed). Regenerate the \
             baseline in the same commit as the toolchain bump: scripts/perf-gate.sh --update \
             (in CI: run the workflow manually, see docs/PERFORMANCE.md)."
        );
        return Ok(if failures.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if let Some(note) = here.soft_differences(&baseline.toolchain) {
        println!(
            "note: environment differs from the baseline's ({note}); small drifts are expected"
        );
    }
    let rows = compare(
        &baseline.benches,
        &measured,
        o.threshold_pct,
        o.only.is_some(),
    );
    print!("{}", render_table(&rows, o.threshold_pct));
    let regressed = rows
        .iter()
        .filter(|r| r.verdict == Verdict::Regressed)
        .count();
    let missing = rows
        .iter()
        .filter(|r| r.verdict == Verdict::Missing)
        .count();
    if rows.iter().any(|r| r.verdict == Verdict::New) {
        println!("new benchmarks are not gated until `scripts/perf-gate.sh --update` records them");
    }
    if regressed > 0 || missing > 0 || !failures.is_empty() {
        println!(
            "FAILED: {regressed} regressed beyond {}%, {missing} missing, {} errors",
            o.threshold_pct,
            failures.len()
        );
        return Ok(ExitCode::FAILURE);
    }
    println!("perf gate passed (threshold {}%)", o.threshold_pct);
    Ok(ExitCode::SUCCESS)
}

fn write_baseline(path: &Path, baseline: &Baseline) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&baseline.to_json())?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(&str, u64)]) -> BTreeMap<String, u64> {
        items.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn tc(rustc: &str) -> Toolchain {
        Toolchain {
            rustc: rustc.into(),
            host: "x86_64-unknown-linux-gnu".into(),
            llvm: "22.1.0".into(),
            profile: "p1".into(),
            valgrind: "valgrind-3.22.0".into(),
            libc: "glibc 2.39".into(),
        }
    }

    #[test]
    fn list_output_is_parsed() {
        let l = parse_list("text/a\twindow\nsearch/b\tprocess\n\nplain\n");
        assert_eq!(l.len(), 3);
        assert_eq!(
            l[0],
            Listed {
                name: "text/a".into(),
                threaded: false
            }
        );
        assert!(l[1].threaded);
        assert!(!l[2].threaded);
    }

    #[test]
    fn callgrind_summary_is_found() {
        let out = "version: 1\ncreator: callgrind-3.22.0\nevents: Ir\nsummary: 326187439\ntotals: 326187439\n";
        assert_eq!(parse_callgrind_summary(out), Some(326_187_439));
        assert_eq!(parse_callgrind_summary("events: Ir\n"), None);
        assert_eq!(parse_callgrind_summary("summary: x\n"), None);
    }

    #[test]
    fn thresholds_are_strict_and_symmetric() {
        let base = map(&[("a", 1000), ("b", 1000), ("c", 1000), ("d", 1000)]);
        let now = map(&[("a", 1050), ("b", 1051), ("c", 949), ("d", 950)]);
        let rows = compare(&base, &now, 5.0, false);
        let verdict = |n: &str| rows.iter().find(|r| r.name == n).map(|r| r.verdict);
        assert_eq!(verdict("a"), Some(Verdict::Ok), "exactly +5% passes");
        assert_eq!(verdict("b"), Some(Verdict::Regressed));
        assert_eq!(verdict("c"), Some(Verdict::Improved));
        assert_eq!(verdict("d"), Some(Verdict::Ok));
        // A looser threshold lets b through.
        let rows = compare(&base, &now, 10.0, false);
        assert!(rows.iter().all(|r| r.verdict == Verdict::Ok));
    }

    #[test]
    fn new_and_missing_benchmarks_are_reported() {
        let base = map(&[("old", 10), ("kept", 10)]);
        let now = map(&[("kept", 10), ("fresh", 5)]);
        let rows = compare(&base, &now, 5.0, false);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().find(|r| r.name == "old").map(|r| r.verdict),
            Some(Verdict::Missing)
        );
        assert_eq!(
            rows.iter().find(|r| r.name == "fresh").map(|r| r.verdict),
            Some(Verdict::New)
        );
        // With a subset run, absent benchmarks are not "missing".
        let rows = compare(&base, &now, 5.0, true);
        assert!(rows.iter().all(|r| r.name != "old"));
    }

    #[test]
    fn zero_baselines_do_not_divide_by_zero() {
        let rows = compare(&map(&[("z", 0)]), &map(&[("z", 7)]), 5.0, false);
        assert_eq!(rows[0].verdict, Verdict::Ok);
    }

    #[test]
    fn baseline_round_trips_with_sorted_keys() {
        let b = Baseline {
            toolchain: tc("1.97.0"),
            benches: map(&[("z/b", 2), ("a/a", 1)]),
        };
        let text = serde_json::to_string_pretty(&b.to_json()).expect("json");
        assert!(text.find("a/a").expect("a") < text.find("z/b").expect("z"));
        assert_eq!(Baseline::from_json(&text).expect("parse"), b);
        assert!(Baseline::from_json("{}").is_err());
        assert!(Baseline::from_json(r#"{"schema":1}"#).is_err());
        assert!(Baseline::from_json(r#"{"schema":2,"benches":{}}"#).is_err());
        assert!(Baseline::from_json(r#"{"schema":1,"benches":{"a":"x"}}"#).is_err());
    }

    #[test]
    fn toolchain_differences_skip_the_comparison() {
        assert_eq!(tc("1.97.0").mismatch(&tc("1.97.0")), None);
        let why = tc("1.98.0").mismatch(&tc("1.97.0")).expect("differs");
        assert!(why.contains("1.97.0") && why.contains("1.98.0"), "{why}");
        let mut other = tc("1.97.0");
        other.profile = "p2".into();
        assert!(tc("1.97.0").mismatch(&other).is_some());
        // Valgrind / libc differences are only noted.
        let mut soft = tc("1.97.0");
        soft.libc = "glibc 2.40".into();
        assert_eq!(soft.mismatch(&tc("1.97.0")), None);
        assert!(soft.soft_differences(&tc("1.97.0")).is_some());
    }

    #[test]
    fn rustc_version_output_is_parsed() {
        let vv = "rustc 1.97.0 (2d8144b78 2026-07-07)\nbinary: rustc\ncommit-hash: 2d8\nhost: x86_64-unknown-linux-gnu\nrelease: 1.97.0\nLLVM version: 22.1.2\n";
        let t = Toolchain::from_rustc_vv(vv, "opt3", "valgrind-3.22.0", "glibc");
        assert_eq!(
            (t.rustc.as_str(), t.host.as_str(), t.llvm.as_str()),
            ("1.97.0", "x86_64-unknown-linux-gnu", "22.1.2")
        );
        assert_eq!(t.profile, "opt3");
    }

    #[test]
    fn table_is_aligned_and_marks_failures() {
        let rows = compare(
            &map(&[("text/a", 1_000_000), ("text/long_name_here", 2_000)]),
            &map(&[("text/a", 1_200_000), ("text/long_name_here", 2_000)]),
            5.0,
            false,
        );
        let table = render_table(&rows, 5.0);
        assert!(table.contains("1,200,000"));
        assert!(table.contains("+20.00%"));
        assert!(table.contains("REGRESSED"));
        let widths: Vec<usize> = table.lines().map(|l| l.find("  ").unwrap_or(0)).collect();
        assert!(widths.iter().all(|w| *w > 0));
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn allowed_cpu_lists() {
        assert_eq!(
            first_allowed_cpu("Name: x\nCpus_allowed_list:\t0-3\n"),
            Some(0)
        );
        assert_eq!(first_allowed_cpu("Cpus_allowed_list: 2,5-7\n"), Some(2));
        assert_eq!(first_allowed_cpu("Cpus_allowed_list: 6\n"), Some(6));
        assert_eq!(first_allowed_cpu("Name: x\n"), None);
    }

    #[test]
    fn arguments() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), None);
        let o = a(&[]).expect("ok").expect("options");
        assert_eq!(o.threshold_pct, 5.0);
        assert!(!o.update);
        let o = a(&[
            "--update",
            "--only",
            "text/",
            "--threshold",
            "7.5%",
            "--repeats",
            "2",
        ])
        .expect("ok")
        .expect("options");
        assert!(
            o.update
                && o.only.as_deref() == Some("text/")
                && o.threshold_pct == 7.5
                && o.repeats == 2
        );
        assert!(a(&["--help"]).expect("ok").is_none());
        assert!(a(&["--threshold"]).is_err());
        assert!(a(&["--threshold", "x"]).is_err());
        assert!(a(&["--repeats", "0"]).is_err());
        assert!(a(&["--nope"]).is_err());
        let from_env = parse_args(&[], Some("12")).expect("ok").expect("options");
        assert_eq!(from_env.threshold_pct, 12.0);
        // The command line wins over the environment.
        let o = parse_args(&["--threshold".into(), "3".into()], Some("12"))
            .expect("ok")
            .expect("options");
        assert_eq!(o.threshold_pct, 3.0);
    }
}
